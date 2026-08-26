//! Administration of one Postgres cluster: how forks and templates are named, created,
//! tagged, found again, and dropped.
//!
//! Every database worktreepg creates carries a `COMMENT ON DATABASE` of the form
//! `worktreepg {json}` (see [`Meta`]). That comment is the only record of ownership, so
//! nothing is ever dropped unless it carries one for the repository at hand.
//!
//! Statements run over a connection to a maintenance database (`postgres`, then `template1`)
//! using the credentials of the first env-file variable that named the database being worked on
//! (see [`Pool`]). The development database itself is never held open here, so it stays usable
//! as a `TEMPLATE`.

use crate::errors::{conflict, conflict_as, environment, environment_as};
use crate::pgurl::PgUrl;
use crate::storage::{self, Sharing};
use anyhow::Result;
use postgres::config::SslMode;
use postgres::error::SqlState;
use postgres::{Client, NoTls};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const META_PREFIX: &str = "worktreepg ";
/// Postgres NAMEDATALEN is 64, so identifiers are at most 63 bytes.
const MAX_IDENTIFIER: usize = 63;

/// The status [`Admin::refused`] gives a statement Postgres refused because the role does not own
/// the database. A caller that has somewhere else to try, or something to say about what would
/// make it work, tells that refusal apart from every other environment error by this.
pub const NOT_OWNER: &str = "not_owner";

/// What a role lacks when Postgres refuses a statement, and the status the refusal is reported
/// under. Everything worktreepg runs is ownership work, except closing someone else's
/// connections: signalling another role's backend is a role membership of its own, which owning
/// the database does not carry. A `DROP DATABASE ... WITH (FORCE)` asks for both, so which of the
/// two it was refused over is decided per failure (see [`Admin::drop_database`]).
#[derive(Clone, Copy)]
struct Refusal {
    status: &'static str,
    advice: &'static str,
}

const NEEDS_OWNERSHIP: Refusal =
    Refusal { status: NOT_OWNER, advice: "That role needs CREATEDB and has to own the database, or be a superuser." };
const NEEDS_SIGNAL: Refusal = Refusal {
    status: "cannot_signal",
    advice:
        "Closing another role's connections needs membership in pg_signal_backend, or superuser; owning the database does not confer it. Stopping whatever is connected needs neither.",
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Meta {
    #[serde(rename_all = "camelCase")]
    Fork {
        v: u8,
        /// Canonical common `.git` directory of the repository.
        repo: PathBuf,
        /// The live development database everything descends from.
        source: String,
        /// What the fork was copied from: `source` when nothing was connected to it, else the template.
        template: String,
        /// Absolute path of the worktree the fork was made for, with symlinks resolved. Every path
        /// it is compared against comes from `git::canonical`, which resolves them the same way,
        /// so the comparison is an equality test and not a guess.
        worktree: PathBuf,
        branch: Option<String>,
        created_at: String,
    },
    #[serde(rename_all = "camelCase")]
    Template { v: u8, repo: PathBuf, source: String, created_at: String },
}

impl Meta {
    pub fn repo(&self) -> &Path {
        match self {
            Meta::Fork { repo, .. } | Meta::Template { repo, .. } => repo,
        }
    }

    pub fn source(&self) -> &str {
        match self {
            Meta::Fork { source, .. } | Meta::Template { source, .. } => source,
        }
    }

    /// The worktree a fork was made for. A template belongs to the repository rather than to any
    /// one worktree, so it has none.
    pub fn worktree(&self) -> Option<&Path> {
        match self {
            Meta::Fork { worktree, .. } => Some(worktree),
            Meta::Template { .. } => None,
        }
    }

    pub fn created_at(&self) -> &str {
        match self {
            Meta::Fork { created_at, .. } | Meta::Template { created_at, .. } => created_at,
        }
    }

    pub fn encode(&self) -> String {
        format!("{META_PREFIX}{}", serde_json::to_string(self).expect("meta serializes"))
    }

    /// `None` for comments that are not ours, including a future format version.
    pub fn decode(comment: &str) -> Option<Self> {
        let json = comment.strip_prefix(META_PREFIX)?;
        let meta: Meta = serde_json::from_str(json).ok()?;
        match meta {
            Meta::Fork { v: 1, .. } | Meta::Template { v: 1, .. } => Some(meta),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Managed {
    pub name: String,
    pub meta: Meta,
}

/// `<source>_<worktree name>`, lowercased with anything outside `[a-z0-9]` turned into `_`, so
/// the name never needs quoting in psql. The full branch name is used rather than its last
/// segment so `feature/auth` and `bugfix/auth` do not share a database.
pub fn fork_name(source: &str, worktree_name: &str) -> Result<String> {
    let mut suffix = String::new();
    let mut pending_sep = false;
    for ch in worktree_name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !suffix.is_empty() {
                suffix.push('_');
            }
            pending_sep = false;
            suffix.push(ch);
        } else {
            pending_sep = true;
        }
    }
    if suffix.is_empty() {
        anyhow::bail!("worktree name \"{worktree_name}\" leaves nothing usable in a database name");
    }
    Ok(truncate_identifier(format!("{source}_{suffix}")))
}

/// `<source>_template`: the snapshot forks are cloned from.
pub fn template_name(source: &str) -> String {
    truncate_identifier(format!("{source}_template"))
}

fn truncate_identifier(mut name: String) -> String {
    while name.len() > MAX_IDENTIFIER {
        name.pop();
    }
    name.trim_end_matches('_').to_string()
}

pub struct ForkSpec {
    pub source: String,
    pub name: String,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ForkOptions {
    /// Drop and re-clone a fork that already exists.
    pub recreate: bool,
    /// Close connections to the live database so it can be cloned while the app is running,
    /// instead of falling back to the template.
    pub terminate: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkStatus {
    /// The fork was cloned, or in a dry run would be.
    Forked {
        from: String,
        copy: CopyMethod,
        origin: Origin,
    },
    Exists,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Nothing was connected to the live database, so the fork is a copy of it as of now. When a
    /// template exists it was replaced with a copy of the fork, so it is just as current.
    Live { template_refreshed: bool },
    /// The live database was in use, so the fork is a copy of the template, taken at `created_at`.
    /// From a dry run this is a prediction rather than something Postgres arbitrated, and an
    /// [`Attached::AtMost`] can predict the template where a real run copies the live database.
    Template { attached: Attached, created_at: String },
}

/// What [`Admin::connections`] found attached to a database, and whether the role that looked can
/// believe the number. `pg_stat_activity` reports `backend_type` as NULL for a session whose role
/// this one holds neither the privileges of nor those of `pg_read_all_stats`, and the count keeps
/// a row it cannot identify, so such a row may be an autovacuum worker rather than one of the
/// app's backends. Masking can only ever inflate the number, never hide a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attached {
    Exactly(i64),
    /// No more than this many, and never 0: a row that will not say what it is still counts, so
    /// a count of zero is exact for every role.
    AtMost(i64),
}

impl Attached {
    /// `masked` is how many of the `n` rows would not say what they were, which is where a worker
    /// the count means to leave out can hide. Masking is decided per row, so it is measured with
    /// the count rather than asked of the role: a role is shown the type of every session it
    /// holds the privileges of, which for a single-role development cluster is all of them.
    fn counted(n: i64, masked: i64) -> Self {
        if masked == 0 {
            Self::Exactly(n)
        } else {
            Self::AtMost(n)
        }
    }

    /// How many backends are attached, where the role can tell.
    pub fn count(self) -> Option<i64> {
        match self {
            Self::Exactly(n) => Some(n),
            Self::AtMost(_) => None,
        }
    }

    /// The most backends that can be attached, which is what a test for "anything at all" needs.
    pub fn upper(self) -> i64 {
        match self {
            Self::Exactly(n) | Self::AtMost(n) => n,
        }
    }
}

/// How `CREATE DATABASE ... TEMPLATE` copies the template's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// Block-by-block through the write-ahead log: Postgres's default, and all that is
    /// available before Postgres 18.
    WalLog,
    /// `STRATEGY = FILE_COPY` with `file_copy_method = clone`: the kernel copies whole files and,
    /// on a copy-on-write filesystem, shares their blocks with the template.
    Clone,
}

impl CopyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyMethod::WalLog => "wal_log",
            CopyMethod::Clone => "clone",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TemplateOptions {
    /// Replace an existing template (refresh) rather than leaving it alone (create).
    pub replace: bool,
    /// Take over a database of the template's name that worktreepg did not create.
    pub force: bool,
    pub terminate: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateStatus {
    Created,
    Replaced,
    Exists,
    Dropped,
    Missing,
}

pub struct Admin {
    client: Client,
    copy: CopyMethod,
    /// Whether the server runs on this host, so its data directory can be inspected.
    local: bool,
    /// The role every statement here runs as, and the cluster it runs on, for diagnostics.
    role: String,
    server: String,
    /// The other roles the directives offer for the work at hand, sorted. The advice on a refused
    /// statement names all of them rather than the one that would have worked: it is built from
    /// what the failure carried, without going back to the server. [`Admin::owner_of`] is that
    /// question, asked ahead of the statement by the caller whose answer changes which role runs
    /// it rather than only what the error says.
    others: Vec<String>,
}

struct ExistingTemplate {
    name: String,
    created_at: String,
}

enum CopyOutcome {
    Copied,
    /// This much was attached to the template, so Postgres refused.
    InUse(Attached),
}

/// Whether a message about a database in use reports a copy Postgres refused or one worktreepg
/// expects it to refuse. A count taken before anything is copied only predicts the refusal, and
/// an [`Attached::AtMost`] can predict it wrong: an autovacuum worker is a row this role cannot
/// identify, and not a reason for Postgres to refuse anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Postgres refused the copy.
    Refused,
    /// Nothing has been copied, and the count is all worktreepg consulted.
    Predicted,
}

/// What is attached to `database`, for the messages about a copy Postgres refused and about one
/// worktreepg expects it to refuse, `basis` saying which. `attached` comes from
/// [`Admin::connections`], which does not see everything a copy is refused over, so a count of
/// zero is not the same as nothing being attached.
pub fn attached(database: &str, attached: Attached, basis: Basis) -> String {
    match attached {
        Attached::Exactly(0) => format!("something is attached to {database} that is not an ordinary connection (a prepared transaction, or a worker Postgres clears itself)"),
        Attached::Exactly(1) => format!("{database} has 1 open connection"),
        Attached::Exactly(n) => format!("{database} has {n} open connections"),
        // The row exists either way. Whether it blocks a copy is the part a prediction cannot
        // say, since the row may be a worker Postgres clears itself.
        Attached::AtMost(_) => match basis {
            Basis::Refused => format!("{database} is in use by something this role cannot identify"),
            Basis::Predicted => format!("{database} looks to be in use by something this role cannot identify"),
        },
    }
}

fn in_use(database: &str, attached_to: Attached, basis: Basis) -> anyhow::Error {
    let remedy = match attached_to {
        Attached::Exactly(0) => "Retrying may be enough; a prepared transaction has to be committed or rolled back first.",
        Attached::Exactly(_) => "Stop the app using it, or re-run with --terminate to close those connections.",
        // Which remedy applies is the part this role cannot see, and --terminate is not offered
        // plainly: closing a session that belongs to another role needs pg_signal_backend, and
        // one refusal fails the statement for every backend in it.
        Attached::AtMost(_) => {
            "pg_stat_activity says what a session is only to a role holding that session's role's privileges, or those of pg_read_all_stats, so the app and an autovacuum worker look the same here: stop the app using it, or retry, which is enough for a worker Postgres clears itself. Closing another role's connections with --terminate needs membership in pg_signal_backend."
        }
    };
    let refusal = match basis {
        Basis::Refused => "so Postgres will not copy it",
        Basis::Predicted => "so Postgres would not copy it",
    };
    environment(format!("{}, {refusal}. {remedy}", attached(database, attached_to, basis)))
}

impl Admin {
    pub fn connect(url: &PgUrl) -> Result<Self> {
        let mut config = postgres::Config::from_str(&url.raw).map_err(|e| environment(format!("{}: {e}", url.server())))?;
        // TLS is not negotiated: this tool targets local development clusters.
        config.ssl_mode(SslMode::Disable);
        let mut errors = Vec::new();
        for maintenance_db in ["postgres", "template1"] {
            config.dbname(maintenance_db);
            match config.connect(NoTls) {
                Ok(mut client) => {
                    let copy = enable_clone(&mut client)?;
                    return Ok(Self {
                        client,
                        copy,
                        local: url.is_local(),
                        role: url.user.clone(),
                        server: url.server(),
                        others: Vec::new(),
                    });
                }
                Err(e) => {
                    let missing = e.code() == Some(&SqlState::INVALID_CATALOG_NAME);
                    errors.push(format!("{maintenance_db}: {e}"));
                    if !missing {
                        break;
                    }
                }
            }
        }
        Err(environment(format!("cannot connect to {}{} (tried {})", url.server(), as_role(&url.user), errors.join("; "))))
    }

    /// Runs a statement, describing it as `action` if Postgres refuses.
    fn run(&mut self, action: &str, sql: &str) -> Result<()> {
        self.client.batch_execute(sql).map_err(|e| self.refused(action, NEEDS_OWNERSHIP, e))
    }

    /// The error for a statement the server rejected. A permission failure names the role,
    /// because statements run as the role in the first directive URL that named the database,
    /// which is not necessarily the role in the variable being applied, and says what `needs`
    /// that role would have to have. The other roles the directives offer are named after it:
    /// the advice is built from what the failure carried, without going back to the server, so it
    /// names all of them rather than the one that would have worked. [`Admin::owner_of`] is that
    /// question, asked ahead of the statement by the caller whose answer changes which role runs
    /// it rather than only what the error says.
    fn refused(&self, action: &str, needs: Refusal, e: postgres::Error) -> anyhow::Error {
        let denied = e.as_db_error().filter(|db| db.code() == &SqlState::INSUFFICIENT_PRIVILEGE).map(|db| db.message().to_string());
        let Some(message) = denied else { return anyhow::Error::new(e).context(action.to_string()) };
        let mut advice = needs.advice.to_string();
        if !self.others.is_empty() {
            let others = self.others.iter().map(|r| format!("\"{r}\"")).collect::<Vec<_>>().join(", ");
            advice.push_str(&format!(
                " Statements on a database run as the role in the first directive URL that names it; the other URLs for it connect as {others}, so if one of those owns it, list its URL first."
            ));
        }
        environment_as(
            needs.status,
            vec![("role", json!(self.role))],
            format!("{action}{} on {}: {message}. {advice}", as_role(&self.role), self.server),
        )
    }

    pub fn copy_method(&self) -> CopyMethod {
        self.copy
    }

    /// One line describing what the copy method will do to disk usage, for `--verbose`. The line
    /// is advisory, and `SHOW data_directory` needs superuser or `pg_read_all_settings`, which
    /// the role that owns the development database need not have, so a server that will not
    /// answer, for want of privileges or for any other reason, costs the detail rather than the
    /// command.
    pub fn storage_note(&mut self) -> String {
        if self.copy == CopyMethod::WalLog {
            return "copy method: wal_log (file cloning needs Postgres 18); forks take as much space as the template".into();
        }
        let data_directory: String = match self.client.query_one("SHOW data_directory", &[]) {
            Ok(row) => row.get(0),
            Err(e) if e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE) => {
                return format!(
                    "copy method: clone; the data directory is not readable{}, so whether forks share blocks with the template is unknown",
                    as_role(&self.role)
                )
            }
            Err(e) => return format!("copy method: clone; could not read the data directory: {e}"),
        };
        let path = Path::new(&data_directory);
        let fs = if self.local { storage::filesystem(path) } else { None };
        match fs {
            Some(fs) => match fs.sharing {
                Sharing::Shared => format!("copy method: clone; {data_directory} is on {}, forks share blocks with the template", fs.name),
                Sharing::Depends => format!(
                    "copy method: clone; {data_directory} is on {}, forks share blocks with the template if the filesystem was created with reflink/block cloning enabled",
                    fs.name
                ),
                Sharing::Copied => format!("copy method: clone; {data_directory} is on {}, which cannot share blocks, so forks are full copies", fs.name),
            },
            None => format!(
                "copy method: clone; {data_directory} is not visible from this host (a container or another machine), so whether forks share blocks depends on its filesystem"
            ),
        }
    }

    pub fn exists(&mut self, name: &str) -> Result<bool> {
        let row = self.client.query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])?;
        Ok(row.is_some())
    }

    /// The role that owns `name`, or `None` when there is no such database. `pg_database` is
    /// world-readable, so this answers for any role, including one that owns nothing on the
    /// cluster.
    pub fn owner_of(&mut self, name: &str) -> Result<Option<String>> {
        let row = self.client.query_opt("SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = $1", &[&name])?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Whether this connection's role has the privileges of `name`'s owner, which is the check
    /// Postgres makes before it lets a role drop or alter a database: a superuser and a member of
    /// the owning role both pass it, where comparing role names would not. False where there is
    /// no such database.
    fn owns(&mut self, name: &str) -> Result<bool> {
        let row = self.client.query_opt("SELECT pg_has_role(datdba, 'USAGE') FROM pg_database WHERE datname = $1", &[&name])?;
        Ok(row.is_some_and(|r| r.get(0)))
    }

    /// Metadata of a database, or `None` when it does not exist or was not created by worktreepg.
    pub fn meta(&mut self, name: &str) -> Result<Option<Meta>> {
        let row = self.client.query_opt(
            "SELECT s.description FROM pg_database d JOIN pg_shdescription s ON s.objoid = d.oid AND s.classoid = 'pg_database'::regclass WHERE d.datname = $1",
            &[&name],
        )?;
        Ok(row.and_then(|r| Meta::decode(r.get::<_, String>(0).as_str())))
    }

    /// Every database on the cluster that carries worktreepg metadata.
    pub fn list_managed(&mut self) -> Result<Vec<Managed>> {
        let rows = self.client.query(
            "SELECT d.datname, s.description FROM pg_database d JOIN pg_shdescription s ON s.objoid = d.oid AND s.classoid = 'pg_database'::regclass WHERE s.description LIKE $1 ORDER BY d.datname",
            &[&format!("{META_PREFIX}%")],
        )?;
        Ok(rows.iter().filter_map(|r| Meta::decode(r.get::<_, String>(1).as_str()).map(|meta| Managed { name: r.get(0), meta })).collect())
    }

    pub fn forks_for(&mut self, repo: &Path) -> Result<Vec<Managed>> {
        Ok(self.list_managed()?.into_iter().filter(|m| matches!(m.meta, Meta::Fork { .. }) && m.meta.repo() == repo).collect())
    }

    /// Makes sure `spec.name` exists as a fork of `spec.source`. The fork is cloned from the live
    /// database when nothing else is connected to it, and from the template otherwise, because
    /// Postgres refuses to copy a database that is in use. A fork cloned from the live database
    /// also replaces the template, when there is one, with a copy of itself, so the fallback is
    /// as current as the last fork. Re-running is a no-op. A database of that name that
    /// worktreepg did not create for this repository, or that records a different worktree, is a
    /// conflict, with or without `recreate`: the recorded worktree may be a live one whose name
    /// normalizes to the same fork name, and recreating then drops its data. With `recreate` the
    /// existing fork is dropped only once the source looks copyable, so a run that cannot make
    /// the replacement keeps the fork it had.
    pub fn ensure_fork(&mut self, spec: &ForkSpec, opts: ForkOptions) -> Result<ForkStatus> {
        // Read before the fork is dropped below, because whether there is a snapshot to clone
        // from is what decides whether dropping it can be made good.
        let template = self.template_of(&spec.source)?;
        if self.exists(&spec.name)? {
            match self.meta(&spec.name)? {
                Some(Meta::Template { .. }) => {
                    return Err(conflict_as(
                        "template",
                        Vec::new(),
                        format!("\"{}\" is a worktreepg template database, not a fork", spec.name),
                    ))
                }
                // The recorded path is only compared here, never followed, so a worktree that has
                // moved or been removed is indistinguishable from a second live worktree whose
                // name normalizes the same way. The message names the remedy for each, and what
                // prune costs: it drops the fork rather than re-pointing it at the new path.
                Some(Meta::Fork { ref repo, ref worktree, .. }) if repo == &spec.repo => {
                    if worktree != &spec.worktree {
                        return Err(conflict_as(
                            "other_worktree",
                            vec![("worktree", json!(worktree))],
                            format!(
                                "\"{}\" is the fork for worktree {}, not {}. If that worktree moved here, moving it back keeps the fork and its data. \"git worktreepg prune\" clears a record whose worktree is gone, but drops the fork and its data with it, and the next apply makes a fresh one. If both worktrees are live, their names produce the same database name: rename one, or pass --worktree-name.",
                                spec.name,
                                worktree.display(),
                                spec.worktree.display()
                            ),
                        ));
                    }
                }
                _ => return Err(conflict("database already exists and was not created by worktreepg for this repository")),
            }
            if !opts.recreate {
                return Ok(ForkStatus::Exists);
            }
            if !opts.dry_run {
                // The fork is only dropped once the source looks copyable, the way a template
                // refresh keeps the snapshot it had: with nothing to fall back on, a --recreate
                // that runs into a connected app would otherwise cost the fork and its data and
                // put nothing in its place. The count is an upper bound rather than what Postgres
                // itself tests, so a bound is treated as connections: refusing a copy Postgres
                // would have made keeps the fork, where taking the bound for nothing would drop
                // it before the copy fails.
                if template.is_none() && !opts.terminate {
                    let attached = self.connections(&spec.source)?;
                    if attached.upper() > 0 {
                        return Err(in_use(&spec.source, attached, Basis::Predicted));
                    }
                }
                self.drop_database(&spec.name)?;
            }
        }

        let copy = self.copy;
        let fallback = |template: Option<ExistingTemplate>, attached: Attached, basis: Basis| match template {
            Some(t) => Ok((t.name, Origin::Template { attached, created_at: t.created_at })),
            None => Err(in_use(&spec.source, attached, basis)),
        };

        if opts.dry_run {
            // A dry run has to predict what Postgres would arbitrate, since asking it would mean
            // creating the database. The prediction only asks whether anything is attached, so an
            // upper bound decides it, and decides it wrong where the bound is all workers: the
            // messages say the prediction is one, and a real run copies the live database.
            let attached = if opts.terminate { Attached::Exactly(0) } else { self.connections(&spec.source)? };
            let (from, origin) = if attached.upper() == 0 {
                (spec.source.clone(), Origin::Live { template_refreshed: template.is_some() })
            } else {
                fallback(template, attached, Basis::Predicted)?
            };
            return Ok(ForkStatus::Forked { from, copy, origin });
        }

        let (from, origin) = match self.create_from(&spec.name, &spec.source, opts.terminate)? {
            CopyOutcome::Copied => (spec.source.clone(), Origin::Live { template_refreshed: template.is_some() }),
            CopyOutcome::InUse(attached) => {
                let (from, origin) = fallback(template, attached, Basis::Refused)?;
                if let CopyOutcome::InUse(n) = self.create_from(&spec.name, &from, false)? {
                    return Err(in_use(&from, n, Basis::Refused));
                }
                (from, origin)
            }
        };
        self.set_meta(
            &spec.name,
            &Meta::Fork {
                v: 1,
                repo: spec.repo.clone(),
                source: spec.source.clone(),
                template: from.clone(),
                worktree: spec.worktree.clone(),
                branch: spec.branch.clone(),
                created_at: now(),
            },
        )?;
        if let Origin::Live { template_refreshed: true } = origin {
            // Copied from the fork rather than the live database: the fork has no connections yet,
            // so this cannot fail the way a second copy of the live database could if the app
            // reconnected in between.
            let name = template_name(&spec.source);
            self.drop_database(&name)?;
            self.create_template(&name, &spec.name, &spec.source, &spec.repo, false)?;
        }
        Ok(ForkStatus::Forked { from, copy, origin })
    }

    /// Snapshots `source` into its template database. The template is flagged `IS_TEMPLATE`, so
    /// any role with `CREATEDB` can clone it and a plain `DROP DATABASE` refuses, and
    /// `ALLOW_CONNECTIONS false`, so it can never be in use when a fork is being made.
    pub fn snapshot_template(&mut self, source: &str, repo: &Path, opts: TemplateOptions) -> Result<TemplateStatus> {
        let name = template_name(source);
        let exists = self.check_template_ownership(&name, opts.force)?;
        if exists && !opts.replace {
            return Ok(TemplateStatus::Exists);
        }
        if opts.dry_run {
            return Ok(if exists { TemplateStatus::Replaced } else { TemplateStatus::Created });
        }
        if exists {
            // The old snapshot is only dropped once the live database looks copyable, so a refresh
            // that runs into a connected app keeps the snapshot it had. A bound is treated as
            // connections for that reason: refusing a refresh that would have worked keeps the
            // snapshot, and taking the bound for nothing would drop it before the copy fails.
            if !opts.terminate {
                let attached = self.connections(source)?;
                if attached.upper() > 0 {
                    return Err(in_use(source, attached, Basis::Predicted));
                }
            }
            self.drop_database(&name)?;
        }
        self.create_template(&name, source, source, repo, opts.terminate)?;
        Ok(if exists { TemplateStatus::Replaced } else { TemplateStatus::Created })
    }

    /// Creates `name` as a copy of `from`, tagged and flagged as the template snapshotted from
    /// `source`. `from` is `source` itself or a fresh fork of it.
    fn create_template(&mut self, name: &str, from: &str, source: &str, repo: &Path, terminate: bool) -> Result<()> {
        if let CopyOutcome::InUse(n) = self.create_from(name, from, terminate)? {
            return Err(in_use(from, n, Basis::Refused));
        }
        self.set_meta(name, &Meta::Template { v: 1, repo: repo.to_path_buf(), source: source.to_string(), created_at: now() })?;
        let sql = format!("ALTER DATABASE {} WITH IS_TEMPLATE true ALLOW_CONNECTIONS false", ident(name));
        self.run(&format!("flagging database {name} as a template"), &sql)?;
        Ok(())
    }

    /// The template snapshotted from `source`, if worktreepg created one.
    fn template_of(&mut self, source: &str) -> Result<Option<ExistingTemplate>> {
        let name = template_name(source);
        Ok(match self.meta(&name)? {
            Some(Meta::Template { created_at, .. }) => Some(ExistingTemplate { name, created_at }),
            _ => None,
        })
    }

    /// Backends attached to `database` other than this one, for the message explaining a copy
    /// Postgres refused. Autovacuum workers are excluded: a shared catalog such as `pg_database`
    /// is vacuumed under whichever database the worker attached to, and Postgres cancels those
    /// workers itself rather than refusing the copy, so counting one would report an app that is
    /// not running and advise stopping it. Everything else a copy blocks on (walsenders,
    /// background workers an extension runs in a database) is counted.
    ///
    /// The number describes the situation rather than reproducing what Postgres tested, and can
    /// be 0 for a database it refused to copy. `pg_stat_activity` shows `backend_type` as NULL
    /// for a session this role may not read the statistics of, so the exclusion is written
    /// NULL-safe: a row that will not say what it is counts, which is why the app's own backends
    /// are counted at all. Those rows are also where an excluded worker hides, so they are
    /// counted a second time and the result is a bound rather than a count when there are any
    /// (see [`Attached`]). A prepared transaction, meanwhile, blocks a copy with no row here to
    /// count (see [`attached`]).
    fn connections(&mut self, database: &str) -> Result<Attached> {
        let row = self.client.query_one(
            "SELECT count(*) FILTER (WHERE backend_type IS DISTINCT FROM 'autovacuum worker'), count(*) FILTER (WHERE backend_type IS NULL) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&database],
        )?;
        Ok(Attached::counted(row.get(0), row.get(1)))
    }

    pub fn drop_template(&mut self, source: &str, opts: TemplateOptions) -> Result<TemplateStatus> {
        let name = template_name(source);
        if !self.check_template_ownership(&name, opts.force)? {
            return Ok(TemplateStatus::Missing);
        }
        if !opts.dry_run {
            self.drop_database(&name)?;
        }
        Ok(TemplateStatus::Dropped)
    }

    /// Drops a fork. Callers pass names from [`Admin::forks_for`], so worktreepg made it for this
    /// repository; whether this connection's role owns it is a separate question, and the only
    /// answer to it is the server's, which comes as a refusal carrying [`NOT_OWNER`].
    pub fn drop_fork(&mut self, name: &str, dry_run: bool) -> Result<()> {
        if !dry_run {
            self.drop_database(name)?;
        }
        Ok(())
    }

    /// The refusal a drop of `name`, owned by `owner`, would meet here, for a dry run, which runs
    /// no statement to be refused by. Ownership is the whole of what can be asked ahead of the
    /// drop, and `pg_has_role` is the check the drop itself makes, so this is the server's answer
    /// rather than a guess from the role names: a superuser and a member of `owner` both pass it.
    /// What is attached to the database is a question for the moment the drop runs, so `None` is
    /// not a promise that it will go through.
    pub fn drop_refusal(&mut self, name: &str, owner: &str) -> Result<Option<anyhow::Error>> {
        if self.owns(name)? {
            return Ok(None);
        }
        Ok(Some(environment_as(
            NOT_OWNER,
            vec![("role", json!(self.role))],
            format!("dropping database {name}{} on {}: \"{owner}\" owns it, so Postgres will refuse it.", as_role(&self.role), self.server),
        )))
    }

    /// Whether the template exists; an existing database of that name that is not our template
    /// is a conflict unless `force`.
    fn check_template_ownership(&mut self, name: &str, force: bool) -> Result<bool> {
        if !self.exists(name)? {
            return Ok(false);
        }
        match self.meta(name)? {
            Some(Meta::Template { .. }) => Ok(true),
            _ if force => Ok(true),
            _ => Err(conflict(format!("{name} exists but was not created by worktreepg (use --force to take it over)"))),
        }
    }

    /// `CREATE DATABASE name TEMPLATE template`. Postgres checks for other backends in the
    /// template before it copies anything, so an in-use outcome has created nothing.
    fn create_from(&mut self, name: &str, template: &str, terminate: bool) -> Result<CopyOutcome> {
        if terminate {
            // Autovacuum workers are left alone, the way connections() does not count them:
            // Postgres clears them itself, and one signal Postgres refuses fails the statement for
            // every backend in it. A role pg_stat_activity masks the view for cannot tell a worker
            // apart, so this spares only the roles that could have signalled one.
            let sql = "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND backend_type IS DISTINCT FROM 'autovacuum worker' AND pid <> pg_backend_pid()";
            self.client
                .execute(sql, &[&template])
                .map_err(|e| self.refused(&format!("closing connections to {template}"), NEEDS_SIGNAL, e))?;
        }
        let strategy = match self.copy {
            CopyMethod::WalLog => "",
            CopyMethod::Clone => " STRATEGY = FILE_COPY",
        };
        let sql = format!("CREATE DATABASE {} TEMPLATE {}{strategy}", ident(name), ident(template));
        match self.client.batch_execute(&sql) {
            Ok(()) => Ok(CopyOutcome::Copied),
            Err(e) if e.code() == Some(&SqlState::OBJECT_IN_USE) => Ok(CopyOutcome::InUse(self.connections(template)?)),
            Err(e) => Err(self.refused(&format!("creating database {name}"), NEEDS_OWNERSHIP, e)),
        }
    }

    fn set_meta(&mut self, name: &str, meta: &Meta) -> Result<()> {
        let sql = format!("COMMENT ON DATABASE {} IS {}", ident(name), literal(&meta.encode()));
        self.run(&format!("commenting on database {name}"), &sql)
    }

    /// `DROP DATABASE ... WITH (FORCE)`, clearing the template flag first when needed.
    ///
    /// `FORCE` closes whatever is still attached, and Postgres refuses a backend this role may not
    /// signal with the same `INSUFFICIENT_PRIVILEGE` it refuses a drop by a role that does not own
    /// the database with. The two want different remedies, and only one of them leaves the caller
    /// another role to try, so which it was is put to the server: a role that got as far as the
    /// signal had already passed the drop's ownership check. A server that will not answer that
    /// leaves the refusal reported as the ownership one.
    fn drop_database(&mut self, name: &str) -> Result<()> {
        let is_template: bool =
            self.client.query_opt("SELECT datistemplate FROM pg_database WHERE datname = $1", &[&name])?.is_some_and(|r| r.get(0));
        let action = format!("dropping database {name}");
        if is_template {
            let sql = format!("ALTER DATABASE {} WITH IS_TEMPLATE false ALLOW_CONNECTIONS true", ident(name));
            self.run(&action, &sql)?;
        }
        let Err(e) = self.client.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", ident(name))) else {
            return Ok(());
        };
        let signalling = e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE) && self.owns(name).unwrap_or(false);
        Err(self.refused(&action, if signalling { NEEDS_SIGNAL } else { NEEDS_OWNERSHIP }, e))
    }
}

/// The admin connections a command uses, one per (cluster, role).
///
/// Which role a statement runs as is decided by the database it is on, not by the variable that
/// led to it: the credentials are the first directive URL naming that database, so a repository
/// whose app connects as a runtime role that owns nothing does all its administration as the
/// privileged URL listed above it. Work is deduplicated by (cluster, database), so each physical
/// database is forked, snapshotted, or dropped once however many variables point at it.
///
/// One connection per cluster is not enough for that. A cluster can hold two databases owned by
/// two different roles, and no ordering of the directives gives one role both, so each database's
/// own owner connects and the connections are pooled per role.
pub struct Pool {
    admins: HashMap<String, Admin>,
    /// The distinct roles the directives offer for a database key or a cluster key. The ones
    /// other than the role that ran a refused statement are named in the error, because
    /// reordering the directives would put one of them in its place.
    roles: HashMap<String, HashSet<String>>,
}

impl Pool {
    /// Takes every directive URL, in the order they were read, so first-seen-wins holds and the
    /// alternatives to each choice are known.
    pub fn new<'a>(urls: impl IntoIterator<Item = &'a PgUrl>) -> Self {
        let mut roles: HashMap<String, HashSet<String>> = HashMap::new();
        for url in urls {
            roles.entry(url.database_key()).or_default().insert(url.user.clone());
            roles.entry(url.cluster_key.clone()).or_default().insert(url.user.clone());
        }
        Self { admins: HashMap::new(), roles }
    }

    /// The connection for work on `database`, on `url`'s cluster. `url` supplies the credentials:
    /// pass the URL from [`crate::project::databases`], which is the first that named the
    /// database. A fork whose source database no directive names any more has no such URL, so
    /// the cluster's is passed instead, and naming `database` separately keeps the advice on a
    /// refused statement about the database the statement is on.
    pub fn for_database(&mut self, url: &PgUrl, database: &str) -> Result<&mut Admin> {
        let scope = url.database_key_of(database);
        self.connect(url, &scope)
    }

    /// The connection for work that spans a cluster: listing what worktreepg manages, finding a
    /// repository's forks. Pass the URL from [`crate::project::clusters`]. Anything destructive
    /// that comes out of such a scan runs on the connection for the database it touches.
    pub fn for_cluster(&mut self, url: &PgUrl) -> Result<&mut Admin> {
        let scope = url.cluster_key.clone();
        self.connect(url, &scope)
    }

    fn connect(&mut self, url: &PgUrl, scope: &str) -> Result<&mut Admin> {
        let key = url.role_key();
        if !self.admins.contains_key(&key) {
            self.admins.insert(key.clone(), Admin::connect(url)?);
        }
        let admin = self.admins.get_mut(&key).expect("inserted above");
        admin.others = match self.roles.get(scope) {
            Some(roles) => {
                let mut others: Vec<String> = roles.iter().filter(|r| *r != &admin.role).cloned().collect();
                others.sort();
                others
            }
            None => Vec::new(),
        };
        Ok(admin)
    }
}

/// Turns on file cloning for the session when the server offers it (Postgres 18+). The
/// setting must be issued on its own: bundling it with `CREATE DATABASE` in one simple-query
/// string would wrap both in a transaction, which `CREATE DATABASE` refuses.
fn enable_clone(client: &mut Client) -> Result<CopyMethod> {
    let row = client.query_opt("SELECT enumvals FROM pg_settings WHERE name = 'file_copy_method'", &[])?;
    let supported = row.is_some_and(|r| r.get::<_, Vec<String>>(0).iter().any(|v| v == "clone"));
    if !supported {
        return Ok(CopyMethod::WalLog);
    }
    client.batch_execute("SET file_copy_method = clone")?;
    Ok(CopyMethod::Clone)
}

/// ` as "role"` for a message, or nothing at all when the URL named no role and libpq falls back
/// to the operating-system user, which worktreepg never learns.
fn as_role(role: &str) -> String {
    if role.is_empty() {
        String::new()
    } else {
        format!(" as \"{role}\"")
    }
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_names_use_the_full_branch() {
        assert_eq!(fork_name("app", "feature/Auth-v2").unwrap(), "app_feature_auth_v2");
        assert_eq!(fork_name("app", "bugfix/auth").unwrap(), "app_bugfix_auth");
        assert_eq!(fork_name("app", "--weird--").unwrap(), "app_weird");
        assert!(fork_name("app", "///").is_err());
    }

    #[test]
    fn names_fit_in_63_bytes() {
        assert_eq!(fork_name("app", &"x".repeat(100)).unwrap().len(), 63);
        assert_eq!(template_name("app"), "app_template");
    }

    #[test]
    fn meta_round_trips_through_a_comment() {
        let meta = Meta::Fork {
            v: 1,
            repo: "/r/.git".into(),
            source: "app".into(),
            template: "app_template".into(),
            worktree: "/r-wt".into(),
            branch: Some("x".into()),
            created_at: "2026-08-22T00:00:00.000Z".into(),
        };
        let comment = meta.encode();
        assert!(comment.starts_with("worktreepg {\"kind\":\"fork\""));
        assert!(comment.contains("\"createdAt\""));
        assert_eq!(Meta::decode(&comment), Some(meta));
    }

    #[test]
    fn meta_ignores_foreign_comments() {
        assert_eq!(Meta::decode("production database"), None);
        assert_eq!(Meta::decode("worktreepg not json"), None);
        assert_eq!(Meta::decode("worktreepg {\"kind\":\"fork\",\"v\":2,\"repo\":\"/\",\"source\":\"a\",\"template\":\"a\",\"worktree\":\"/\",\"branch\":null,\"createdAt\":\"\"}"), None);
    }

    #[test]
    fn quoting() {
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(literal("it's"), "'it''s'");
    }
}
