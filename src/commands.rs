//! The five commands. Each one works in terms of forks, templates, and worktrees; the SQL,
//! metadata, and file-format details live in `db`, `envfile`, and `project`.

use crate::db::{self, CopyMethod, ForkOptions, ForkSpec, ForkStatus, Meta, Origin, Pool, TemplateOptions, TemplateStatus};
use crate::envfile::EnvFile;
use crate::errors::{environment, is_conflict, usage, EXIT_CONFLICT, EXIT_INTERNAL};
use crate::git;
use crate::pgurl::PgUrl;
use crate::project::{clusters, databases, EnvVar, ProblemKind, Project};
use crate::report::{Counts, Reporter};
use anyhow::Result;
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

/// The connection that drops a fork of `source`, which a scan turned up: the credentials of the
/// first directive URL that names `source` on this cluster. A fork whose source database the
/// directives no longer name falls back to the cluster's first URL, which is all that is left to
/// try, and the work is still scoped to `source` rather than to whatever that URL names.
fn dropper<'a>(pool: &'a mut Pool, live: &[PgUrl], cluster: &PgUrl, source: &str) -> Result<&'a mut db::Admin> {
    let url = live.iter().find(|u| u.cluster_key == cluster.cluster_key && u.database == source).unwrap_or(cluster);
    pool.for_database(url, source)
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
/// the edit the next time it ran.
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

    let finish = |counts: &mut Counts, reporter: &Reporter| -> i32 {
        if opts.dry_run {
            counts.rename("created", "create_planned");
            counts.rename("template_refreshed", "template_refresh_planned");
            counts.rename("rewritten", "rewrite_planned");
        }
        reporter.finish(doc.clone(), counts);
        if counts.get("errors") > 0 {
            EXIT_INTERNAL
        } else if counts.get("conflicts") > 0 {
            EXIT_CONFLICT
        } else {
            0
        }
    };

    if project.target == project.source {
        reporter.info(format!("{} is the source worktree; nothing to fork", project.target.display()));
        return Ok(finish(&mut counts, reporter));
    }

    let (vars, problems) = project.env_vars()?;
    for p in &problems {
        if p.kind == ProblemKind::InvalidUrl {
            counts.inc("errors");
            reporter.action(
                json!({ "op": "error", "path": p.file, "var": p.name, "status": "invalid_url" }),
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
    for file in by_file.keys() {
        if !project.target.join(file).is_file() {
            return Err(environment(format!(
                "{file} does not exist in {} yet. Run \"git worktreeinclude apply\" there first; worktreepg only edits env files it finds.",
                project.target.display()
            )));
        }
    }

    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let clusters = clusters(&vars);
    // Every outcome, a conflict and an unusable name included, marks the database handled, so a
    // second variable naming it neither repeats the work nor reports it twice. `forks` holds only
    // the databases that ended up with a fork, which is what the rewrite below needs.
    let mut handled: HashSet<String> = HashSet::new();
    let mut forks: HashMap<String, String> = HashMap::new();
    let mut noted: HashSet<String> = HashSet::new();
    for v in &vars {
        let key = v.url.database_key();
        if !handled.insert(key.clone()) {
            continue;
        }
        let name = match db::fork_name(&v.url.database, &worktree_name) {
            Ok(name) => name,
            Err(e) => {
                counts.inc("errors");
                reporter.action(json!({ "op": "error", "database": v.url.database, "status": "unnameable" }), format!("error     {e}"));
                continue;
            }
        };
        let spec = ForkSpec {
            source: v.url.database.clone(),
            name: name.clone(),
            repo: project.repo.clone(),
            worktree: project.target.clone(),
            branch: branch.clone(),
        };
        let fork_opts = ForkOptions { recreate: opts.recreate, terminate: opts.terminate, dry_run: opts.dry_run };
        note_storage(&mut pool, &clusters, &v.url, &mut noted, reporter);
        match pool.for_database(&v.url, &v.url.database)?.ensure_fork(&spec, fork_opts) {
            Ok(ForkStatus::Forked { from, copy, origin }) => {
                counts.inc("created");
                let st = status(opts.dry_run);
                let mut action = json!({ "op": "create_database", "database": name, "from": from, "copy": copy.as_str(), "status": st });
                if let Origin::Template { connections, .. } = &origin {
                    action["live_connections"] = json!(connections);
                }
                reporter.action(action, format!("create    {name} (from {from}, {}){}", describe_copy(copy), planned(opts.dry_run)));
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
                    Origin::Template { connections, created_at } => reporter.warn(format!(
                        "{}, so {name} {} a copy of {from}, taken {} ago. For current data, stop the app (or pass --terminate) and run apply --recreate.",
                        db::attached(&spec.source, connections),
                        if opts.dry_run { "would be" } else { "is" },
                        age(&created_at),
                    )),
                }
            }
            Ok(ForkStatus::Exists) => {
                counts.inc("skipped_existing");
                reporter.action(json!({ "op": "create_database", "database": name, "status": "exists" }), format!("exists    {name}"));
            }
            Err(e) if is_conflict(&e) => {
                counts.inc("conflicts");
                reporter.action(json!({ "op": "conflict", "database": name, "status": "unmanaged" }), format!("conflict  {name}: {e}"));
                continue;
            }
            Err(e) => return Err(e),
        }
        forks.insert(key, name);
    }

    for (file, file_vars) in &by_file {
        let path = project.target.join(file);
        let mut env = EnvFile::open(&path)?;
        for v in file_vars {
            let Some(fork) = forks.get(&v.url.database_key()) else { continue };
            let current = env.get(&v.name).map(|value| PgUrl::parse(&value).map_or(value, |u| u.database));
            match current.as_deref() {
                Some(db) if db == fork => {
                    counts.inc("skipped_same");
                    reporter.action(
                        json!({ "op": "skip", "path": file, "var": v.name, "status": "same" }),
                        format!("skip      {file} {} (already {fork})", v.name),
                    );
                    continue;
                }
                Some(db) if db != v.url.database && !opts.force => {
                    counts.inc("conflicts");
                    reporter.action(
                        json!({ "op": "conflict", "path": file, "var": v.name, "status": "diff" }),
                        format!("conflict  {file} {} points at \"{db}\", not \"{}\" (use --force)", v.name, v.url.database),
                    );
                    continue;
                }
                _ => {}
            }
            env.set(&v.name, &v.url.with_database(fork));
            counts.inc("rewritten");
            reporter.action(
                json!({ "op": "rewrite", "path": file, "var": v.name, "database": fork, "status": status(opts.dry_run) }),
                format!("rewrite   {file} {} -> {fork}{}", v.name, planned(opts.dry_run)),
            );
        }
        if !opts.dry_run {
            env.save()?;
        }
    }
    Ok(finish(&mut counts, reporter))
}

pub struct RemoveOptions {
    pub path: Option<String>,
    pub keep_worktree: bool,
    pub dry_run: bool,
    pub force: bool,
}

/// Removes a worktree and drops the forks made for it. The worktree goes first so git's own
/// checks (uncommitted changes, locks) run before anything irreversible happens; a fork left
/// behind by a failure is picked up by `prune`.
pub fn remove(project: &Project, opts: &RemoveOptions, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let target = git::canonical(&project.cwd.join(opts.path.as_deref().unwrap_or(".")));
    if target == project.source {
        return Err(usage(format!("{} is the source worktree; refusing to remove it", target.display())));
    }
    let registered = git::living_worktrees(&project.cwd)?.contains(&target);
    let mut counts = Counts::new(&["worktree_removed", "dropped"]);
    let doc = document(vec![("dry_run", json!(opts.dry_run)), ("worktree", json!(target))]);

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

    drop_forks(project, opts.dry_run, reporter, &mut counts, |meta_worktree| meta_worktree == target)?;
    reporter.finish(doc, &counts);
    Ok(0)
}

/// Drops every fork whose worktree git no longer lists: the catch-up after a plain `git worktree remove`.
pub fn prune(project: &Project, dry_run: bool, reporter: &mut Reporter) -> Result<i32> {
    project.require_directives()?;
    let living: HashSet<PathBuf> = git::living_worktrees(&project.cwd)?.into_iter().collect();
    let mut counts = Counts::new(&["forks", "dropped", "kept"]);
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
            dropper(&mut pool, &live, cluster, fork.meta.source())?.drop_fork(&fork.name, dry_run)?;
            counts.inc("dropped");
            reporter.action(
                json!({ "op": "drop_database", "database": fork.name, "worktree": worktree, "status": status(dry_run) }),
                format!("drop      {} (worktree {} is gone{})", fork.name, worktree.display(), if dry_run { ", planned" } else { "" }),
            );
        }
    }
    reporter.finish(document(vec![("dry_run", json!(dry_run))]), &counts);
    Ok(0)
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
            TemplateAction::Drop => admin.drop_template(&url.database, opts),
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

/// Drops the repository's forks whose recorded worktree satisfies `select`.
fn drop_forks(
    project: &Project,
    dry_run: bool,
    reporter: &mut Reporter,
    counts: &mut Counts,
    select: impl Fn(&Path) -> bool,
) -> Result<()> {
    let (vars, problems) = project.env_vars()?;
    warn_all(&problems, reporter);
    let mut pool = Pool::new(vars.iter().map(|v| &v.url));
    let live = databases(&vars);
    for cluster in &clusters(&vars) {
        for fork in pool.for_cluster(cluster)?.forks_for(&project.repo)? {
            let Meta::Fork { worktree, .. } = &fork.meta else { continue };
            if !select(worktree) {
                continue;
            }
            dropper(&mut pool, &live, cluster, fork.meta.source())?.drop_fork(&fork.name, dry_run)?;
            counts.inc("dropped");
            reporter.action(
                json!({ "op": "drop_database", "database": fork.name, "worktree": worktree, "status": status(dry_run) }),
                format!("drop      {}{}", fork.name, planned(dry_run)),
            );
        }
    }
    Ok(())
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
