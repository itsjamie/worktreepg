//! The five commands. Each one works in terms of forks, templates, and worktrees; the SQL,
//! metadata, and file-format details live in `db`, `envfile`, and `project`.

use crate::db::{self, CopyMethod, ForkOptions, ForkSpec, ForkStatus, Managed, Meta, Origin, Pool, TemplateOptions, TemplateStatus};
use crate::envfile::EnvFile;
use crate::errors::{annotated, detail_of, environment, exit_code, is_conflict, usage, EXIT_CONFLICT, EXIT_ENVIRONMENT, EXIT_INTERNAL};
use crate::git;
use crate::pgurl::PgUrl;
use crate::project::{clusters, contained_path, databases, EnvVar, ProblemKind, Project};
use crate::report::{Counts, Reporter};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

fn document(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn planned(dry_run: bool) -> &'static str {
    if dry_run {
        " (planned)"
    } else {
        ""
    }
}

fn describe_copy(copy: CopyMethod) -> &'static str {
    match copy {
        CopyMethod::Clone => "cloned",
        CopyMethod::WalLog => "copied",
    }
}

fn status(dry_run: bool) -> &'static str {
    if dry_run {
        "planned"
    } else {
        "done"
    }
}

/// How long ago an RFC 3339 timestamp was, coarsely: "a moment", "5 minutes", "3 days".
fn age(timestamp: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(timestamp) else { return timestamp.to_string() };
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds().max(0);
    let (n, unit) = match secs {
        0..=59 => return "a moment".to_string(),
        60..=3599 => (secs / 60, "minute"),
        3600..=86399 => (secs / 3600, "hour"),
        _ => (secs / 86400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Which URL a fork of `source` is dropped on: the first directive URL that names `source` on this
/// cluster, so first-seen-wins holds wherever the source is still named, else the cluster's own
/// first URL, which is all that is left to try. The work stays scoped to `source` rather than to
/// whatever the chosen URL names.
fn dropper<'a>(live: &'a [PgUrl], cluster: &'a PgUrl, source: &str) -> &'a PgUrl {
    live.iter().find(|u| u.cluster_key == cluster.cluster_key && u.database == source).unwrap_or(cluster)
}

/// A URL on the cluster that connects as `owner`, for the second attempt at a drop the first role
/// was refused. The role that owns a fork is reached this way wherever any directive offers it,
/// including through a URL naming some other database that role owns, which is what a fork whose
/// own source is no longer named has left.
fn owners_url<'a>(live: &'a [PgUrl], cluster: &PgUrl, tried: &PgUrl, owner: &str) -> Option<&'a PgUrl> {
    live.iter().find(|u| u.cluster_key == cluster.cluster_key && u.user == owner && u.role_key() != tried.role_key())
}

/// What puts a fork's drop back within reach of a role that can do it. Only ownership does, and a
/// URL naming the fork's source database is the one that usually carries the owning role, which is
/// why the source is named as an example rather than as the remedy itself.
fn restore_a_directive(source: &str, owner: Option<&str>) -> String {
    match owner {
        Some(owner) => format!(
            "List a directive URL on this cluster that connects as \"{owner}\", such as the one for \"{source}\", and \"git worktreepg prune\" drops it."
        ),
        None => format!("List a directive URL naming \"{source}\", and \"git worktreepg prune\" drops it."),
    }
}

/// Drops the fork, as the first role and then as the one that owns it where a URL on the cluster
/// connects as that role and the first was refused for want of ownership. Returns the refusal to
/// report, or `None` where the fork was dropped.
fn attempt_drop(pool: &mut Pool, source: &str, name: &str, first: &PgUrl, owning: Option<&PgUrl>) -> Result<Option<anyhow::Error>> {
    let mut refusal = None;
    for url in [Some(first), owning].into_iter().flatten() {
        match pool.for_database(url, source)?.drop_fork(name, false) {
            Ok(()) => return Ok(None),
            Err(e) if is_not_owner(&e) => refusal = Some(e),
            Err(e) => return Err(e),
        }
    }
    Ok(Some(refusal.expect("the first URL was attempted")))
}

/// Whether the server refused a statement because the role does not own the database, which is the
/// one refusal a drop has somewhere else to try after, and can carry on past.
fn is_not_owner(e: &anyhow::Error) -> bool {
    detail_of(e).is_some_and(|d| d.status == db::NOT_OWNER)
}

/// Drops one fork a cluster scan turned up, counting it. A drop refused for want of ownership by
/// every role the directives offer is counted as a skip and reported with the remedy, rather than
/// failing the command: a fork nothing can drop fails every later run the same way, which leaves
/// it unreachable by any command, so the run finishes the rest of its work and reports the one
/// piece it could not do. Returns whether the fork was dropped, because the two callers describe
/// that in their own words.
fn drop_scanned(
    pool: &mut Pool,
    live: &[PgUrl],
    cluster: &PgUrl,
    fork: &Managed,
    dry_run: bool,
    counts: &mut Counts,
    reporter: &mut Reporter,
) -> Result<bool> {
    let source = fork.meta.source();
    let first = dropper(live, cluster, source);
    let owner = pool.for_cluster(cluster)?.owner_of(&fork.name)?;
    let owning = owner.as_deref().and_then(|owner| owners_url(live, cluster, first, owner));

    let refused = if dry_run {
        let admin = pool.for_database(first, source)?;
        // A dry run runs no statement, so the refusal it would meet is asked for instead. It only
        // stands where no URL on the cluster connects as the owning role either, since that one is
        // what a real run falls back to.
        match (&owner, &owning) {
            (Some(owner), None) => admin.drop_refusal(&fork.name, owner)?,
            _ => None,
        }
    } else {
        attempt_drop(pool, source, &fork.name, first, owning)?
    };
    let Some(e) = refused else {
        counts.inc("dropped");
        return Ok(true);
    };

    counts.inc("skipped");
    let mut action = json!({
        "op": "skip",
        "database": fork.name,
        "worktree": fork.meta.worktree(),
        "source": source,
        "owner": owner,
        "status": db::NOT_OWNER,
        "predicted": dry_run,
    });
    if let Some(detail) = detail_of(&e) {
        for (field, value) in &detail.fields {
            action[*field] = value.clone();
        }
    }
    let line = format!("skip      {}{}: {e} {}", fork.name, planned(dry_run), restore_a_directive(source, owner.as_deref()));
    reporter.action_or_warn(action, line);
    Ok(false)
}

/// The `--verbose` line about what a fork will cost on disk, once per cluster, taken on the first
/// URL that named the cluster. It describes the copy method rather than reporting a result, so it
/// is only worth a query when someone is going to read it, and a server that will not answer
/// costs the line rather than the command.
fn note_storage(pool: &mut Pool, clusters: &[PgUrl], url: &PgUrl, noted: &mut HashSet<String>, reporter: &Reporter) {
    if !reporter.is_verbose() || !noted.insert(url.cluster_key.clone()) {
        return;
    }
    let cluster = clusters.iter().find(|c| c.cluster_key == url.cluster_key).unwrap_or(url);
    match pool.for_cluster(cluster) {
        Ok(admin) => reporter.verbose(admin.storage_note()),
        Err(e) => reporter.warn(format!("cannot say how forks will use disk on {}: {e}", cluster.server())),
    }
}

/// Connects to every database the directives name, while the command has still changed nothing.
/// A server that is down or a password that is wrong is one failure for every database on that
/// cluster, so finding it here costs the run nothing; the price is that a directive naming a
/// cluster that is gone for good blocks the command, the reachable clusters included, until the
/// directive or the env file is fixed. `left` names the state the command stopped in, since a
/// bare connection failure reads the same on either side of the work.
fn connect_all(pool: &mut Pool, live: &[PgUrl], left: &str) -> Result<()> {
    for url in live {
        pool.for_database(url, &url.database).map_err(|e| annotated(e, left))?;
    }
    Ok(())
}

/// A reported failure's action, under whatever status and fields the error asked to be reported
/// with in place of the caller's generic ones.
fn detailed(mut action: Value, e: &anyhow::Error) -> Value {
    if let Some(detail) = detail_of(e) {
        action["status"] = json!(detail.status);
        for (field, value) in &detail.fields {
            action[*field] = value.clone();
        }
    }
    action
}

pub struct ApplyOptions {
    pub worktree_name: Option<String>,
    pub recreate: bool,
    pub terminate: bool,
    pub dry_run: bool,
    pub force: bool,
}

/// Forks the database for the current worktree and points its env file at the fork. Meant to
/// run from the new worktree right after `git worktreeinclude apply`, which is what puts the
/// env file there; worktreepg never creates it, because git-worktreeinclude would overwrite
/// the edit the next time it ran. Every decision that does not need the cluster is made first,
/// in `resolve`, so a run that stops on a conflict has created nothing.
pub fn apply(project: &Project, opts: &ApplyOptions, reporter: &mut Reporter) -> Result<i32> {
    let branch = git::current_branch(&project.target)?;
    let worktree_name = match &opts.worktree_name {
        Some(name) => name.clone(),
        None => git::worktree_name(&project.target)?,
    };
    let mut counts = Counts::new(&[
        "matched",
        "created",
        "template_refreshed",
        "skipped_existing",
        "rewritten",
        "skipped_same",
        "skipped_missing_src",
        "conflicts",
        "errors",
    ]);
    let doc = document(vec![
        ("dry_run", json!(opts.dry_run)),
        ("from", json!(project.source)),
        ("to", json!(project.target)),
        ("include_file", json!(project.include_file.strip_prefix(&project.source).unwrap_or(&project.include_file))),
        ("worktree", json!({ "branch": branch, "name": worktree_name })),
    ]);

    // `exit` carries the code a swallowed failure would have exited with, so counting one instead
    // of returning it does not also reclassify it: an unreachable server stays an environment
    // error rather than becoming the internal error `errors` maps to.
    let finish = |counts: &mut Counts, reporter: &Reporter, exit: Option<i32>| -> i32 {
        if opts.dry_run {
            counts.rename("created", "create_planned");
            counts.rename("template_refreshed", "template_refresh_planned");
            counts.rename("rewritten", "rewrite_planned");
        }
        reporter.finish(doc.clone(), counts);
        if let Some(code) = exit {
            code
        } else if counts.get("errors") > 0 {
            EXIT_INTERNAL
        } else if counts.get("conflicts") > 0 {
            EXIT_CONFLICT
        } else {
            0
        }
    };

    if project.target == project.source {
        reporter.info(format!("{} is the source worktree; nothing to fork", project.target.display()));
        return Ok(finish(&mut counts, reporter, None));
    }

    let (vars, problems) = project.env_vars()?;
    let mut exit = None;
    for p in &problems {
        if p.kind == ProblemKind::InvalidUrl {
            counts.inc("errors");
            exit = Some(EXIT_ENVIRONMENT);
            reporter.failure(
                json!({ "op": "error", "path": p.file, "var": p.name, "status": "invalid_url", "message": p.detail }),
                format!("error     {}", p.detail),
            );
        } else {
            counts.inc("skipped_missing_src");
            reporter
                .action(json!({ "op": "skip", "path": p.file, "var": p.name, "status": "missing_src" }), format!("skip      {}", p.detail));
        }
    }
    counts.add("matched", vars.len());

    let mut by_file: BTreeMap<&str, Vec<&EnvVar>> = BTreeMap::new();
    for v in &vars {
        by_file.entry(&v.file).or_default().push(v);
    }

    let Plan { targets, names, blocked } = resolve(project, &by_file, &worktree_name, opts.force, &mut counts, reporter)?;
    if blocked {
        return Ok(finish(&mut counts, reporter, exit));
    }

    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let clusters = clusters(&vars);
    // Taken before the first CREATE DATABASE, so a failure a connection can predict stops the run
    // having made a fork whose variable the rewrite pass would never reach. What no connection
    // could predict, such as a role without CREATEDB, still surfaces one database at a time below.
    connect_all(&mut pool, &databases(&vars), "nothing was created and no env file was written")?;
    let mut tried: HashSet<String> = HashSet::new();
    let mut forks: HashSet<String> = HashSet::new();
    let mut noted: HashSet<String> = HashSet::new();
    // First error wins: an exit code names the kind of thing that went wrong, not how badly, so
    // there is nothing for a later failure of another kind to be worse than.
    for v in &vars {
        let key = v.url.database_key();
        // One attempt per database, however many variables name it. `forks` cannot stand in for
        // this set: a conflict leaves the database out of it, and every later variable naming
        // that database would try again and report the same conflict.
        if !tried.insert(key.clone()) {
            continue;
        }
        let name = names.get(&key).expect("resolve named every database or blocked the run").clone();
        let spec = ForkSpec {
            source: v.url.database.clone(),
            name: name.clone(),
            repo: project.repo.clone(),
            worktree: project.target.clone(),
            branch: branch.clone(),
        };
        let fork_opts = ForkOptions { recreate: opts.recreate, terminate: opts.terminate, dry_run: opts.dry_run };
        note_storage(&mut pool, &clusters, &v.url, &mut noted, reporter);
        // The pre-flight above connected to this database, so this is a cache hit; folding it in
        // keeps the borrow inside the match rather than needing a `?` the loop would return on. A
        // server that goes away mid-run surfaces from the queries inside `ensure_fork`.
        match pool.for_database(&v.url, &v.url.database).and_then(|admin| admin.ensure_fork(&spec, fork_opts)) {
            Ok(ForkStatus::Forked { from, copy, origin }) => {
                counts.inc("created");
                let st = status(opts.dry_run);
                let mut action = json!({ "op": "create_database", "database": name, "from": from, "copy": copy.as_str(), "status": st });
                // The age is read once so the JSON action and the fallback warning below cannot
                // disagree across a second boundary. Both snapshot keys describe when {from} was
                // taken, not when this fork was created, which is what list's created_at means.
                let (origin_field, snapshot_age) = match &origin {
                    Origin::Live { .. } => {
                        action["origin"] = json!("live");
                        ("live", None)
                    }
                    Origin::Template { attached, created_at, .. } => {
                        let snapshot_age = age(created_at);
                        action["origin"] = json!("template");
                        // Only where the role could see a number: a bound would read as a count
                        // to a caller acting on the field, and there is nothing to act on.
                        if let Some(connections) = attached.count() {
                            action["live_connections"] = json!(connections);
                        }
                        action["snapshot_created_at"] = json!(created_at);
                        action["snapshot_age"] = json!(&snapshot_age);
                        ("template", Some(snapshot_age))
                    }
                };
                reporter.action(
                    action,
                    format!("create    {name} (from {from}, {}, origin={origin_field}){}", describe_copy(copy), planned(opts.dry_run)),
                );
                match origin {
                    Origin::Live { template_refreshed: true } => {
                        counts.inc("template_refreshed");
                        let template = db::template_name(&spec.source);
                        reporter.action(
                            json!({ "op": "refresh_template", "database": template, "source": spec.source, "copy": copy.as_str(), "status": st }),
                            format!("refresh   {template} (from {}, {}){}", spec.source, describe_copy(copy), planned(opts.dry_run)),
                        );
                    }
                    Origin::Live { template_refreshed: false } => {}
                    // A real run is here because Postgres refused the live database; a dry run
                    // because the count says it would, which is a prediction it can get wrong.
                    Origin::Template { attached, signals, .. } => {
                        let basis = if opts.dry_run { db::Basis::Predicted } else { db::Basis::Refused };
                        let remedy = match signals {
                            // This run passed --terminate and Postgres refused it on part of what
                            // was attached, so offering the flag would be offering what has just
                            // failed. What is left needs a membership the run has already been
                            // told it lacks, and stopping it needs nothing at all.
                            db::Signals::Denied => "--terminate closed the connections this role can close, and Postgres refused the rest: closing another role's connections needs membership in pg_signal_backend, or superuser, and stopping whatever is connected needs neither. For current data, stop it and run apply --recreate.",
                            db::Signals::Accepted => "For current data, stop the app (or pass --terminate) and run apply --recreate.",
                        };
                        reporter.warn(format!(
                            "{}, so {name} {} a copy of {from}, taken {} ago. {remedy}",
                            db::attached(&spec.source, attached, basis),
                            if opts.dry_run { "would be" } else { "is" },
                            snapshot_age.unwrap_or_default(),
                        ));
                    }
                }
            }
            Ok(ForkStatus::Exists) => {
                counts.inc("skipped_existing");
                reporter.action(json!({ "op": "create_database", "database": name, "status": "exists" }), format!("exists    {name}"));
            }
            // A conflict here cannot stop the run the way a conflict in `resolve` does: the forks
            // for earlier variables already exist, and a fork whose variable was never rewritten
            // is the orphan this ordering exists to prevent.
            Err(e) if is_conflict(&e) => {
                counts.inc("conflicts");
                reporter.action(
                    detailed(json!({ "op": "conflict", "database": name, "status": "unmanaged" }), &e),
                    format!("conflict  {name}: {e}"),
                );
                continue;
            }
            // Nor can any other per-database failure, and for the same reason: returning here
            // would leave every fork made so far pointing at nothing, because the rewrite pass
            // runs only once the loop is done. A database that failed having created nothing
            // stays out of `forks`, so its variables keep naming the live database.
            Err(e) => {
                counts.inc("errors");
                exit.get_or_insert_with(|| exit_code(&e));
                // The whole chain, the way returning it would have printed it: the outermost
                // context describes the statement, and the server's reason for refusing it is
                // underneath. The messages are built out of database names, roles, and servers,
                // so no URL passes through them, and "failed" classifies nothing on its own, so
                // the message travels in the action rather than only in the line.
                let message = format!("{e:#}");
                reporter.failure(
                    detailed(json!({ "op": "error", "database": name, "status": "failed", "message": &message }), &e),
                    format!("error     {name}: {message}"),
                );
                // --recreate drops the fork before it clones the replacement, so a failure past
                // that point has already cost this database its fork and its data, and the error
                // does not say whether it did. The run stops rather than risking the same on
                // every database left; the rewrite pass below still runs, so the forks it did
                // make are reachable, and a second apply picks the rest up.
                if opts.recreate {
                    break;
                }
                continue;
            }
        }
        forks.insert(key);
    }

    for Target { file, mut env, vars } in targets {
        // A file's rewrites are held until its save has been attempted, so nothing is reported as
        // written that the file does not hold: a caller acting on a rewrite, by reading the file
        // back or restarting the service that reads it, would otherwise act on the live URL.
        let mut pending: Vec<(Value, String)> = Vec::new();
        for p in &vars {
            if !forks.contains(&p.source) {
                continue;
            }
            let (v, fork) = (p.var, &p.fork);
            if p.same {
                counts.inc("skipped_same");
                reporter.action(
                    json!({ "op": "skip", "path": file, "var": v.name, "status": "same" }),
                    format!("skip      {file} {} (already {fork})", v.name),
                );
                continue;
            }
            env.set(&v.name, &v.url.with_database(fork));
            pending.push((
                json!({ "op": "rewrite", "path": file, "var": v.name, "database": fork, "status": status(opts.dry_run) }),
                format!("rewrite   {file} {} -> {fork}{}", v.name, planned(opts.dry_run)),
            ));
        }
        let refused = if opts.dry_run { None } else { env.save().err() };
        match refused {
            None => {
                counts.add("rewritten", pending.len());
                for (action, line) in pending {
                    reporter.action(action, line);
                }
            }
            // One unwritable file is not a reason to leave the other files unwritten: each one is
            // a separate rewrite, and the forks behind all of them exist either way. This file's
            // rewrites are reported as not written, and counted as neither.
            Some(e) => {
                let e = anyhow::Error::from(e);
                for (mut action, line) in pending {
                    action["status"] = json!("not_written");
                    reporter.action(action, line);
                }
                counts.inc("errors");
                exit.get_or_insert_with(|| exit_code(&e));
                let message = format!("{e:#}");
                reporter.failure(
                    json!({ "op": "error", "path": file, "status": "failed", "message": &message }),
                    format!("error     {file}: {message}; its rewrites were not written"),
                );
            }
        }
    }
    Ok(finish(&mut counts, reporter, exit))
}

/// What one variable's rewrite will be, decided before anything is created and carried to the
/// rewrite pass, so the file is neither read nor classified a second time.
struct PlannedVar<'a> {
    var: &'a EnvVar,
    /// The variable's live database, keyed as in `Plan::names`: the rewrite is skipped when that
    /// database's fork turned out to be a conflict.
    source: String,
    fork: String,
    /// The file already names the fork, so there is nothing to write. The fork is still ensured,
    /// because the database may have been dropped since the last apply.
    same: bool,
}

/// A target env file kept from the pre-flight to the write so the decision for a file and the
/// write for it cannot disagree. `save` refuses if an outside edit lands while the forks are
/// being cloned, preserving that edit rather than overwriting it with the pre-flight content.
struct Target<'a> {
    file: &'a str,
    env: EnvFile,
    vars: Vec<PlannedVar<'a>>,
}

/// Everything `apply` can settle before it touches the cluster.
struct Plan<'a> {
    targets: Vec<Target<'a>>,
    /// Fork name per live database, keyed by [`PgUrl::database_key`]. Every database the run's
    /// variables name has an entry unless `blocked`.
    names: HashMap<String, String>,
    /// A conflict or an error was reported, so nothing may be created and no file may be written.
    blocked: bool,
}

/// Names the fork for every database the run needs and classifies every target variable against
/// the file it will be rewritten in. Nothing here reaches the cluster: fork names are pure and
/// the current value is in a file. Whatever this refuses, it refuses while `apply` can still exit
/// having created nothing, which matters because a fork whose env file was never rewritten is
/// unreachable: `prune` keeps any fork whose worktree still lives, and no command can name it.
fn resolve<'a>(
    project: &Project,
    by_file: &BTreeMap<&'a str, Vec<&'a EnvVar>>,
    worktree_name: &str,
    force: bool,
    counts: &mut Counts,
    reporter: &mut Reporter,
) -> Result<Plan<'a>> {
    let mut plan = Plan { targets: Vec::new(), names: HashMap::new(), blocked: false };
    let mut opened = Vec::new();
    for (&file, file_vars) in by_file {
        let path = contained_path(&project.target, Path::new(file))
            .ok_or_else(|| environment(format!("{file} resolves outside the target worktree")))?;
        if !path.is_file() {
            return Err(environment(format!(
                "{file} does not exist in {} yet. Run \"git worktreeinclude apply\" there first; worktreepg only edits env files it finds.",
                project.target.display()
            )));
        }
        let env = EnvFile::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
        opened.push((file, file_vars, env));
    }

    let mut tried: HashSet<String> = HashSet::new();
    for v in by_file.values().flatten() {
        let key = v.url.database_key();
        if !tried.insert(key.clone()) {
            continue;
        }
        match db::fork_name(&v.url.database, worktree_name) {
            Ok(name) => {
                plan.names.insert(key, name);
            }
            Err(e) => {
                counts.inc("errors");
                let message = format!("{e:#}");
                reporter.failure(
                    json!({ "op": "error", "database": v.url.database, "status": "unnameable", "message": &message }),
                    format!("error     {message}"),
                );
                plan.blocked = true;
            }
        }
    }

    for (file, file_vars, env) in opened {
        let mut vars = Vec::new();
        for v in file_vars {
            let key = v.url.database_key();
            let Some(fork) = plan.names.get(&key) else { continue };
            let current = env.get(&v.name).map(|value| PgUrl::parse(&value).map_or(value, |u| u.database));
            let same = match current.as_deref() {
                Some(db) if db == fork => true,
                Some(db) if db != v.url.database && !force => {
                    counts.inc("conflicts");
                    reporter.action(
                        json!({ "op": "conflict", "path": file, "var": v.name, "status": "diff" }),
                        format!("conflict  {file} {} points at \"{db}\", not \"{}\" (use --force)", v.name, v.url.database),
                    );
                    plan.blocked = true;
                    continue;
                }
                _ => false,
            };
            vars.push(PlannedVar { var: v, source: key, fork: fork.clone(), same });
        }
        plan.targets.push(Target { file, env, vars });
    }
    Ok(plan)
}

pub struct RemoveOptions {
    pub path: Option<String>,
    pub keep_worktree: bool,
    pub dry_run: bool,
    pub force: bool,
}

/// Removes a worktree and drops the forks made for it. Every database the directives name is
/// connected to before anything is touched, so a server that cannot be reached fails the
/// command while the worktree is still there to re-run it against; the price is that a
/// directive naming a cluster that is gone for good blocks `remove` until the directive or the
/// env file is fixed. Git's own checks (uncommitted changes, locks) still run, and the
/// worktree still goes, before any database is dropped; a fork left behind by a failure past
/// that point is picked up by `prune`. A fork none of the roles the directives offer owns is
/// reported as a skip and the rest are still dropped: a fork nothing can drop fails every later
/// run the same way, which leaves it unreachable by any command, so the run finishes the work it
/// can do and reports the one piece it could not. It exits 4 having skipped one, and a
/// `--dry-run` that predicts a skip reports and exits the same way.
pub fn remove(project: &Project, opts: &RemoveOptions, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let target = opts.path.as_ref().map_or_else(|| project.target.clone(), |path| git::canonical(&project.cwd.join(path)));
    if target == project.source {
        return Err(usage(format!("{} is the source worktree; refusing to remove it", target.display())));
    }
    let registered = git::living_worktrees(&project.cwd)?.contains(&target);
    let mut counts = Counts::new(&["worktree_removed", "dropped", "skipped"]);
    let doc = document(vec![("dry_run", json!(opts.dry_run)), ("worktree", json!(target))]);

    // Every database the directives name, not only the ones holding a fork of this worktree:
    // telling those apart needs the connection anyway. One connection per (cluster, role) is
    // enough to cover the cluster scans below as well, because the first URL naming a cluster is
    // also the first URL naming its own database.
    let (vars, problems) = project.env_vars()?;
    warn_all(&problems, reporter);
    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let live = databases(&vars);
    connect_all(&mut pool, &live, &format!("{} was left in place and no database was dropped", target.display()))?;

    if registered && !opts.keep_worktree {
        if !opts.dry_run {
            git::remove_worktree(&target, opts.force, &project.source)?;
        }
        counts.inc("worktree_removed");
        reporter.action(
            json!({ "op": "remove_worktree", "path": target, "status": status(opts.dry_run) }),
            format!("remove    worktree {}{}", target.display(), planned(opts.dry_run)),
        );
    } else if !registered {
        reporter.verbose(format!("{} is not a registered worktree; dropping its databases only", target.display()));
    }

    for cluster in &clusters(&vars) {
        for fork in pool.for_cluster(cluster)?.forks_for(&project.repo)? {
            let Meta::Fork { worktree, .. } = &fork.meta else { continue };
            if worktree != &target {
                continue;
            }
            if drop_scanned(&mut pool, &live, cluster, &fork, opts.dry_run, &mut counts, reporter)? {
                reporter.action(
                    json!({ "op": "drop_database", "database": fork.name, "worktree": worktree, "status": status(opts.dry_run) }),
                    format!("drop      {}{}", fork.name, planned(opts.dry_run)),
                );
            }
        }
    }
    reporter.finish(doc, &counts);
    Ok(if counts.get("skipped") > 0 { EXIT_ENVIRONMENT } else { 0 })
}

/// Drops every fork whose worktree git no longer lists: the catch-up after a plain `git worktree
/// remove`, and the way back for a fork an earlier `remove` had to skip. One it still cannot drop
/// is skipped again rather than stopping the run, so the forks after it are not held up by it.
pub fn prune(project: &Project, dry_run: bool, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let living: HashSet<PathBuf> = git::living_worktrees(&project.cwd)?.into_iter().collect();
    let mut counts = Counts::new(&["forks", "dropped", "kept", "skipped"]);
    let (vars, problems) = project.env_vars()?;
    warn_all(&problems, reporter);
    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let live = databases(&vars);
    for cluster in &clusters(&vars) {
        for fork in pool.for_cluster(cluster)?.forks_for(&project.repo)? {
            let Meta::Fork { worktree, .. } = &fork.meta else { continue };
            counts.inc("forks");
            if living.contains(worktree) {
                counts.inc("kept");
                reporter.verbose(format!("keep      {} ({})", fork.name, worktree.display()));
                continue;
            }
            if drop_scanned(&mut pool, &live, cluster, &fork, dry_run, &mut counts, reporter)? {
                reporter.action(
                    json!({ "op": "drop_database", "database": fork.name, "worktree": worktree, "status": status(dry_run) }),
                    format!("drop      {} (worktree {} is gone{})", fork.name, worktree.display(), if dry_run { ", planned" } else { "" }),
                );
            }
        }
    }
    reporter.finish(document(vec![("dry_run", json!(dry_run))]), &counts);
    Ok(if counts.get("skipped") > 0 { EXIT_ENVIRONMENT } else { 0 })
}

/// Shows the template and forks on each cluster, and whether each fork's worktree still exists.
pub fn list(project: &Project, all: bool, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let living: HashSet<PathBuf> = git::living_worktrees(&project.cwd)?.into_iter().collect();
    let mut rows = Vec::new();
    let (vars, problems) = project.env_vars()?;
    warn_all(&problems, reporter);
    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    for cluster in &clusters(&vars) {
        let server = cluster.server();
        for m in pool.for_cluster(cluster)?.list_managed()? {
            if !all && m.meta.repo() != project.repo {
                continue;
            }
            match &m.meta {
                Meta::Template { .. } => {
                    reporter.info(format!("template  {}  from {}  refreshed {}", m.name, m.meta.source(), m.meta.created_at()));
                    rows.push(json!({ "server": server, "database": m.name, "kind": "template", "source": m.meta.source(), "repo": m.meta.repo(), "created_at": m.meta.created_at() }));
                }
                Meta::Fork { worktree, branch, template, .. } => {
                    let present = living.contains(worktree);
                    reporter.info(format!(
                        "fork      {}  {}{}",
                        m.name,
                        worktree.display(),
                        if present { "" } else { "  (worktree missing, run prune)" }
                    ));
                    rows.push(json!({
                        "server": server, "database": m.name, "kind": "fork", "source": m.meta.source(), "template": template,
                        "repo": m.meta.repo(), "worktree": worktree, "branch": branch, "worktree_exists": present, "created_at": m.meta.created_at(),
                    }));
                }
            }
        }
    }
    if rows.is_empty() {
        reporter.info("no worktreepg databases found");
    }
    let mut counts = Counts::default();
    counts.add("databases", rows.len());
    reporter.finish(document(vec![("databases", Value::Array(rows))]), &counts);
    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateAction {
    Create,
    Refresh,
    Drop,
}

pub struct TemplateCommand {
    pub action: TemplateAction,
    pub terminate: bool,
    pub dry_run: bool,
    pub force: bool,
}

/// Manages the snapshot forks are cloned from. `create` takes it from the live development
/// database, `refresh` replaces it with a fresh copy, `drop` removes it. Existing forks are
/// untouched by a refresh; re-fork a worktree with `apply --recreate`.
pub fn template(project: &Project, cmd: &TemplateCommand, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let mut counts = Counts::new(&["created", "dropped", "skipped", "conflicts"]);
    let (vars, problems) = project.env_vars()?;
    warn_all(&problems, reporter);
    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let clusters = clusters(&vars);
    let mut noted: HashSet<String> = HashSet::new();
    let opts = TemplateOptions {
        replace: cmd.action == TemplateAction::Refresh,
        force: cmd.force,
        terminate: cmd.terminate,
        dry_run: cmd.dry_run,
    };
    for url in &databases(&vars) {
        let name = db::template_name(&url.database);
        if cmd.action != TemplateAction::Drop {
            note_storage(&mut pool, &clusters, url, &mut noted, reporter);
        }
        let admin = pool.for_database(url, &url.database)?;
        let copy = admin.copy_method();
        let result = match cmd.action {
            TemplateAction::Drop => admin.drop_template(&url.database, &project.repo, opts),
            _ => admin.snapshot_template(&url.database, &project.repo, opts),
        };
        let st = status(cmd.dry_run);
        match result {
            Ok(TemplateStatus::Created) => {
                counts.inc("created");
                reporter.action(
                    json!({ "op": "create_database", "database": name, "source": url.database, "copy": copy.as_str(), "status": st }),
                    format!("create    {name} (from {}, {}){}", url.database, describe_copy(copy), planned(cmd.dry_run)),
                );
            }
            Ok(TemplateStatus::Replaced) => {
                counts.inc("dropped");
                counts.inc("created");
                reporter.action(
                    json!({ "op": "drop_database", "database": name, "status": st }),
                    format!("drop      {name}{}", planned(cmd.dry_run)),
                );
                reporter.action(
                    json!({ "op": "create_database", "database": name, "source": url.database, "copy": copy.as_str(), "status": st }),
                    format!("create    {name} (from {}, {}){}", url.database, describe_copy(copy), planned(cmd.dry_run)),
                );
            }
            Ok(TemplateStatus::Dropped) => {
                counts.inc("dropped");
                reporter.action(
                    json!({ "op": "drop_database", "database": name, "status": st }),
                    format!("drop      {name}{}", planned(cmd.dry_run)),
                );
            }
            Ok(TemplateStatus::Exists) => {
                counts.inc("skipped");
                reporter.action(
                    json!({ "op": "skip", "database": name, "status": "exists" }),
                    format!("exists    {name} (use \"template refresh\" to rebuild it)"),
                );
            }
            Ok(TemplateStatus::Missing) => {
                counts.inc("skipped");
                reporter.action(json!({ "op": "skip", "database": name, "status": "missing" }), format!("skip      {name} does not exist"));
            }
            Err(e) if is_conflict(&e) => {
                counts.inc("conflicts");
                reporter.action(json!({ "op": "conflict", "database": name, "status": "unmanaged" }), format!("conflict  {e}"));
            }
            Err(e) => return Err(e),
        }
    }
    let action = match cmd.action {
        TemplateAction::Create => "create",
        TemplateAction::Refresh => "refresh",
        TemplateAction::Drop => "drop",
    };
    reporter.finish(document(vec![("dry_run", json!(cmd.dry_run)), ("action", json!(action))]), &counts);
    Ok(if counts.get("conflicts") > 0 { EXIT_CONFLICT } else { 0 })
}

/// Problems reading the source env files are warnings for the database-management commands.
fn warn_all(problems: &[crate::project::Problem], reporter: &Reporter) {
    for p in problems {
        reporter.warn(&p.detail);
    }
}

#[cfg(test)]
mod tests {
    use super::age;
    use chrono::{Duration, Utc};

    fn ago(d: Duration) -> String {
        (Utc::now() - d).to_rfc3339()
    }

    #[test]
    fn age_is_coarse_and_pluralized() {
        assert_eq!(age(&ago(Duration::seconds(5))), "a moment");
        assert_eq!(age(&ago(Duration::seconds(61))), "1 minute");
        assert_eq!(age(&ago(Duration::minutes(45))), "45 minutes");
        assert_eq!(age(&ago(Duration::hours(3))), "3 hours");
        assert_eq!(age(&ago(Duration::days(3))), "3 days");
        assert_eq!(age("not a timestamp"), "not a timestamp");
    }
}
