//! End-to-end: the real binary against a real git repository with worktrees and a real
//! Postgres cluster. Needs `WORKTREE_PG_TEST_URL` pointing at a superuser on a maintenance
//! database of a cluster running with autovacuum off, which is what the drops made as a plain
//! owner and the exact connection counts asserted on here need (scripts/test-db.sh starts one,
//! and says why); skips otherwise.

use postgres::{Client, NoTls};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use worktreepg::pgurl::{redact, with_database};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    json: Value,
}

fn run(args: &[&str], cwd: &Path, json: bool) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git-worktreepg"));
    cmd.args(args).current_dir(cwd);
    if json {
        cmd.arg("--json");
    }
    let out = cmd.output().expect("run worktreepg");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    Run { code: out.status.code().unwrap_or(-1), stdout, stderr, json }
}

fn git(args: &[&str], cwd: &Path) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Db {
    admin: Client,
    admin_url: String,
}

impl Db {
    /// Drops whatever a previous run left behind under `source` and creates it fresh. Tests run
    /// in parallel against one cluster, so each picks a source name of its own.
    fn connect(url: &str, source: &str) -> Self {
        // url_as rebuilds this URL as another role, which it can only do by replacing a
        // user:password authority. Any other form (a unix socket, no credentials) would leave a
        // URL pointing at libpq's default server rather than at the one the test set up.
        assert!(
            url.split_once("://").and_then(|(_, rest)| rest.split_once('@')).is_some_and(|(userinfo, _)| userinfo.contains(':')),
            "WORKTREE_PG_TEST_URL must be postgres://user:password@host:port/database, not {}",
            redact(url)
        );
        let mut admin = Client::connect(url, NoTls).expect("connect to WORKTREE_PG_TEST_URL");
        let rows = admin
            .query("SELECT datname FROM pg_database WHERE datname = $1 OR datname LIKE $2", &[&source, &format!("{source}\\_%")])
            .unwrap();
        for name in rows.iter().map(|r| r.get::<_, String>(0)) {
            admin.batch_execute(&format!("ALTER DATABASE \"{name}\" WITH IS_TEMPLATE false")).ok();
            admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")).unwrap();
        }
        admin.batch_execute(&format!("CREATE DATABASE \"{source}\"")).unwrap();
        Self { admin, admin_url: url.to_string() }
    }

    fn url(&self, database: &str) -> String {
        with_database(&self.admin_url, database)
    }

    /// The same URL as the admin one, as another role, with the password [`Db::create_role`]
    /// gives it. Only the credentials and the database change, so a port, an `sslmode`, or
    /// anything else in `WORKTREE_PG_TEST_URL` is carried over rather than dropped.
    fn url_as(&self, role: &str, database: &str) -> String {
        let url = self.url(database);
        let (scheme, rest) = url.split_once("://").expect("scheme");
        let (_, tail) = rest.split_once('@').expect("credentials, checked in connect");
        format!("{scheme}://{role}:pw@{tail}")
    }

    /// A role that can log in and nothing else: no CREATEDB, and it owns no database.
    fn role(&mut self, name: &str) {
        self.create_role(name, "NOCREATEDB");
    }

    /// A role that owns `database` and can fork it, the way a per-service owner does on a cluster
    /// that holds several development databases.
    fn owner_of(&mut self, role: &str, database: &str) {
        self.create_role(role, "CREATEDB");
        self.admin.batch_execute(&format!("CREATE DATABASE \"{database}\" OWNER \"{role}\"")).unwrap();
    }

    fn create_role(&mut self, name: &str, createdb: &str) {
        self.admin
            .batch_execute(&format!(
                "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{name}') THEN CREATE ROLE \"{name}\" LOGIN PASSWORD 'pw' {createdb}; END IF; END $$"
            ))
            .unwrap();
    }

    fn grant(&mut self, role: &str, group: &str) {
        self.admin.batch_execute(&format!("GRANT \"{group}\" TO \"{role}\"")).unwrap();
    }

    /// Cleanup goes by database name, so a role a test creates has to be dropped by that test,
    /// after the databases it could own are gone.
    fn drop_role(&mut self, name: &str) {
        self.admin.batch_execute(&format!("DROP ROLE IF EXISTS \"{name}\"")).unwrap();
    }

    fn owner(&mut self, database: &str) -> String {
        self.admin.query_one("SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = $1", &[&database]).unwrap().get(0)
    }

    fn client(&self, database: &str) -> Client {
        Client::connect(&self.url(database), NoTls).unwrap()
    }

    fn exists(&mut self, name: &str) -> bool {
        self.admin.query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&name]).unwrap().is_some()
    }

    fn flags(&mut self, name: &str) -> (bool, bool) {
        let row = self.admin.query_one("SELECT datistemplate, datallowconn FROM pg_database WHERE datname = $1", &[&name]).unwrap();
        (row.get(0), row.get(1))
    }

    fn meta(&mut self, name: &str) -> Value {
        let row = self
            .admin
            .query_one("SELECT s.description FROM pg_database d JOIN pg_shdescription s ON s.objoid = d.oid WHERE d.datname = $1", &[&name])
            .unwrap();
        let comment: String = row.get(0);
        serde_json::from_str(comment.strip_prefix("worktreepg ").expect("worktreepg comment")).unwrap()
    }

    /// Waits for every backend on `database` to go away. Dropping a `Client` closes its socket
    /// without waiting for the backend to exit, and both worktreepg's connection counts and its
    /// choice between the live database and the template come from `pg_stat_activity`, so a test
    /// that depends on either has to let the last connection go first.
    fn wait_idle(&mut self, database: &str) {
        for _ in 0..100 {
            // Same boundary Admin::connections draws: an autovacuum worker is not something a
            // test can wait out, and Postgres terminates one itself rather than refusing a copy.
            let sql = "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND backend_type IS DISTINCT FROM 'autovacuum worker' AND pid <> pg_backend_pid()";
            let row = self.admin.query_one(sql, &[&database]).unwrap();
            if row.get::<_, i64>(0) == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("connections to {database} never went away");
    }

    fn things(&self, database: &str) -> Vec<String> {
        self.client(database).query("SELECT name FROM things ORDER BY name", &[]).unwrap().iter().map(|r| r.get(0)).collect()
    }
}

fn env_database(worktree: &Path) -> String {
    let content = fs::read_to_string(worktree.join(".env")).unwrap();
    let line = content.lines().find(|l| l.starts_with("DATABASE_URL=")).unwrap();
    let after_host = line.rsplit('/').next().unwrap();
    after_host.split(['?', '"']).next().unwrap().to_string()
}

fn summary(r: &Run, key: &str) -> u64 {
    r.json["summary"][key].as_u64().unwrap_or_else(|| panic!("summary.{key} missing in {}\n{}", r.stdout, r.stderr))
}

/// The message on the first `op` action, which is where `--json` carries the reason a database
/// was not forked. A run that reports the failure rather than returning it prints its line on
/// stderr, but under `--json` stdout is the only stream, so this is all such a caller has.
fn message(r: &Run, op: &str) -> String {
    let actions = r.json["actions"].as_array().unwrap_or_else(|| panic!("no actions in {}\n{}", r.stdout, r.stderr));
    let action = actions.iter().find(|a| a["op"] == op).unwrap_or_else(|| panic!("no {op} action in {}", r.stdout));
    action["message"].as_str().unwrap_or_else(|| panic!("no message on {op} in {}", r.stdout)).to_string()
}

/// The test cluster, or `None` when there is none and the test should skip.
fn test_url(test: &str) -> Option<String> {
    match std::env::var("WORKTREE_PG_TEST_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping {test}: set WORKTREE_PG_TEST_URL (see scripts/test-db.sh)");
            None
        }
    }
}

/// A repository whose `.worktreeinclude` names `vars` in `.env`, with a source `.env` pointing
/// all of them at `live_url`. Returns the repository root and that `.env`, which is what
/// `git worktreeinclude apply` would copy into a new worktree.
fn fixture(root: &Path, vars: &[&str], live_url: &str) -> (PathBuf, String) {
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\n.worktreeinclude\n").unwrap();
    fs::write(repo.join(".worktreeinclude"), format!("# worktreepg: .env {}\n.env\n", vars.join(" "))).unwrap();
    let mut env: String = vars.iter().map(|v| format!("{v}=\"{live_url}?sslmode=disable\"\n")).collect();
    env.push_str("OTHER=keep\n");
    fs::write(repo.join(".env"), &env).unwrap();
    fs::write(repo.join("README.md"), "app\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);
    (repo, env)
}

/// An env variable that already points at a third database stops the run before any database is
/// created, and leaves the file it appears in untouched even where its siblings are rewritable.
#[test]
fn an_env_conflict_stops_the_run_before_anything_is_created() {
    let Some(admin_url) = test_url("an_env_conflict_stops_the_run_before_anything_is_created") else { return };
    let mut db = Db::connect(&admin_url, "envc");
    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let (repo, source_env) = fixture(&root, &["DATABASE_URL", "DIRECT_URL"], &db.url("envc"));

    let wt = root.join("wt-conflict");
    git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "conflict"], &repo);
    let conflicted =
        format!("DATABASE_URL=\"{}\"\nDIRECT_URL=\"{}?sslmode=disable\"\nOTHER=keep\n", db.url("envc_elsewhere"), db.url("envc"));
    fs::write(wt.join(".env"), &conflicted).unwrap();

    let r = run(&["apply"], &wt, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(summary(&r, "conflicts"), 1);
    assert_eq!(summary(&r, "created"), 0);
    assert_eq!(summary(&r, "rewritten"), 0);
    let conflict = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "conflict").unwrap();
    assert_eq!(conflict["var"], "DATABASE_URL");
    assert_eq!(conflict["status"], "diff");
    assert!(!db.exists("envc_conflict"), "no database is created while a variable is in conflict");
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), conflicted, "DIRECT_URL is rewritable, but the file is left alone");

    // --dry-run reports the same conflict, which is what makes a rehearsal unnecessary
    let r = run(&["apply", "--dry-run"], &wt, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(summary(&r, "conflicts"), 1);

    // --force still overrides it, and both variables are then rewritten
    let r = run(&["apply", "--force"], &wt, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "rewritten"), 2);
    assert!(db.exists("envc_conflict"));
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), source_env.replace(&db.url("envc"), &db.url("envc_conflict")));
}

/// Two worktree names that normalize to one fork name are a conflict, not one shared database.
/// Two variables name that database, which is one attempt and so one conflict.
#[test]
fn a_fork_belonging_to_another_worktree_is_never_adopted() {
    let Some(admin_url) = test_url("a_fork_belonging_to_another_worktree_is_never_adopted") else { return };
    let mut db = Db::connect(&admin_url, "coll");
    db.client("coll").batch_execute("CREATE TABLE things (name text PRIMARY KEY); INSERT INTO things VALUES ('one')").unwrap();
    db.wait_idle("coll");
    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let (repo, source_env) = fixture(&root, &["DATABASE_URL", "DIRECT_URL"], &db.url("coll"));

    let dash = root.join("wt-dash");
    git(&["worktree", "add", "-q", dash.to_str().unwrap(), "-b", "feature/auth-v2"], &repo);
    fs::write(dash.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &dash, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    db.client("coll_feature_auth_v2").batch_execute("INSERT INTO things VALUES ('dash')").unwrap();

    // feature/auth.v2 differs from feature/auth-v2 only outside [a-z0-9], so both name one fork
    let dot = root.join("wt-dot");
    git(&["worktree", "add", "-q", dot.to_str().unwrap(), "-b", "feature/auth.v2"], &repo);
    fs::write(dot.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &dot, false);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(r.stdout.contains(dash.to_str().unwrap()), "the conflict names the worktree that owns the fork: {}", r.stdout);
    assert!(r.stdout.contains(dot.to_str().unwrap()), "the conflict names this worktree: {}", r.stdout);

    // --recreate does not turn it into permission to drop the other worktree's database
    let r = run(&["apply", "--recreate"], &dot, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(summary(&r, "conflicts"), 1, "one database, however many variables name it");
    assert_eq!(summary(&r, "rewritten"), 0);
    let conflict = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "conflict").unwrap();
    assert_eq!(conflict["status"], "other_worktree");
    assert_eq!(conflict["worktree"], dash.to_str().unwrap(), "the action names the worktree that owns the fork");
    assert_eq!(db.things("coll_feature_auth_v2"), ["dash", "one"]);
    assert_eq!(db.meta("coll_feature_auth_v2")["worktree"], dash.to_str().unwrap());
    assert_eq!(env_database(&dot), "coll");
}

/// A worktree that moved records a path nothing lives at any more, which is the same mismatch a
/// second live worktree of a colliding name produces. The fork is refused either way, and the
/// message names both remedies: moving the worktree back, which keeps the fork, and prune, which
/// clears the record by dropping the fork.
#[test]
fn a_moved_worktree_is_a_conflict_that_names_its_remedies() {
    let Some(admin_url) = test_url("a_moved_worktree_is_a_conflict_that_names_its_remedies") else { return };
    let mut db = Db::connect(&admin_url, "movd");
    db.client("movd").batch_execute("CREATE TABLE things (name text PRIMARY KEY); INSERT INTO things VALUES ('one')").unwrap();
    db.wait_idle("movd");
    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let (repo, source_env) = fixture(&root, &["DATABASE_URL"], &db.url("movd"));

    let before = root.join("wt-before");
    git(&["worktree", "add", "-q", before.to_str().unwrap(), "-b", "moved"], &repo);
    fs::write(before.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &before, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    db.client("movd_moved").batch_execute("INSERT INTO things VALUES ('two')").unwrap();

    // the branch is what the fork is named after, so the move changes only the recorded path
    let after = root.join("wt-after");
    git(&["worktree", "move", before.to_str().unwrap(), after.to_str().unwrap()], &repo);
    let r = run(&["apply"], &after, false);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(r.stdout.contains("moving it back"), "the conflict names the remedy that keeps the data: {}", r.stdout);
    assert!(r.stdout.contains("git worktreepg prune"), "the conflict names the command that clears the record: {}", r.stdout);
    assert!(r.stdout.contains("drops the fork"), "and says what prune costs: {}", r.stdout);
    assert!(r.stdout.contains(before.to_str().unwrap()), "the conflict names the recorded worktree: {}", r.stdout);

    let r = run(&["apply"], &after, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(summary(&r, "conflicts"), 1);
    assert_eq!(summary(&r, "rewritten"), 0);
    let conflict = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "conflict").unwrap();
    assert_eq!(conflict["status"], "other_worktree", "worktreepg manages this fork for this repository");
    assert_eq!(db.things("movd_moved"), ["one", "two"], "the refusal leaves the fork's data where it is");
    assert_eq!(env_database(&after), "movd_moved");
}

/// Same credentials and database as `url`, but a port nothing listens on, so connecting fails
/// immediately instead of hanging. Credentials are optional: a cluster reached under trust or
/// peer auth has none.
fn unreachable(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("url has a scheme");
    let (authority, database) = rest.split_once('/').expect("url has a database");
    let credentials = authority.rsplit_once('@').map_or(String::new(), |(before_at, _)| format!("{before_at}@"));
    format!("{scheme}://{credentials}127.0.0.1:1/{database}")
}

#[test]
fn end_to_end() {
    let Some(admin_url) = test_url("end_to_end") else { return };
    let mut db = Db::connect(&admin_url, "app");
    let live_url = db.url("app");
    let mut live = db.client("app");
    live.batch_execute("CREATE TABLE things (name text PRIMARY KEY); INSERT INTO things VALUES ('one')").unwrap();
    // closed rather than dropped: the next apply refuses to copy a database anything is connected
    // to. close() returns once this end is shut down; wait_idle waits for the backend to leave
    // pg_stat_activity, which is what worktreepg actually counts.
    live.close().unwrap();
    db.wait_idle("app");

    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let repo = root.join("app");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\n.worktreeinclude\n").unwrap();
    fs::write(repo.join(".worktreeinclude"), "# worktreepg: .env DATABASE_URL\n.env\n").unwrap();
    let source_env = format!("DATABASE_URL=\"{live_url}?sslmode=disable\"\nOTHER=keep\n");
    fs::write(repo.join(".env"), &source_env).unwrap();
    fs::write(repo.join("README.md"), "app\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);

    // apply in the source worktree is a no-op
    let r = run(&["apply"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "matched"), 0);

    // apply waits for git-worktreeinclude to have put the env file in place
    let auth = root.join("app-auth");
    git(&["worktree", "add", "-q", auth.to_str().unwrap(), "-b", "feature/auth"], &repo);
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("git worktreeinclude apply"), "{}", r.stderr);
    assert!(!db.exists("app_feature_auth"), "nothing is created until the env file exists");

    // apply forks the live database and rewrites .env in the new worktree
    fs::write(auth.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json["worktree"], serde_json::json!({ "branch": "feature/auth", "name": "feature/auth" }));
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "rewritten"), 1);
    let version: i32 = db.admin.query_one("SHOW server_version_num", &[]).unwrap().get::<_, String>(0).parse().unwrap();
    let create = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "create_database").unwrap();
    assert_eq!(create["copy"], if version >= 180000 { "clone" } else { "wal_log" });
    assert_eq!(create["origin"], "live");
    assert!(create.get("snapshot_age").is_none(), "{create}");
    assert!(create.get("snapshot_created_at").is_none(), "{create}");
    let meta = db.meta("app_feature_auth");
    assert_eq!(meta["kind"], "fork");
    assert_eq!(meta["worktree"], auth.to_str().unwrap());
    assert_eq!(meta["branch"], "feature/auth");
    assert_eq!(meta["template"], "app");
    assert_eq!(
        fs::read_to_string(auth.join(".env")).unwrap(),
        format!("DATABASE_URL=\"{}?sslmode=disable\"\nOTHER=keep\n", db.url("app_feature_auth"))
    );
    assert_eq!(db.things("app_feature_auth"), ["one"]);

    // origin=live also shows up in the terse per-line output that downstream scripts parse,
    // not just the --json shape
    let live_origin = root.join("app-live-origin");
    git(&["worktree", "add", "-q", live_origin.to_str().unwrap(), "-b", "live-origin"], &repo);
    fs::write(live_origin.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &live_origin, false);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains(", origin=live)"), "{}", r.stdout);
    let r = run(&["remove", live_origin.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!db.exists("app_live_origin"));

    // apply is idempotent
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "skipped_existing"), 1);
    assert_eq!(summary(&r, "skipped_same"), 1);

    // without a template, a live database in use cannot be forked at all
    db.wait_idle("app");
    {
        let _holder = db.client("app");
        let r = run(&["apply", "--recreate"], &auth, true);
        assert_eq!(r.code, 4, "{}", r.stderr);
        assert_eq!(summary(&r, "errors"), 1);
        let failure = message(&r, "error");
        assert!(failure.contains("1 open connection"), "{failure}");
        assert!(failure.contains("--terminate"), "{failure}");
        let r = run(&["apply", "--recreate", "--dry-run"], &auth, true);
        assert_eq!(r.code, 4, "{}", r.stderr);
    }
    // and the fork it could not replace is still there, with its data: --recreate refuses before
    // it drops rather than after, so the run costs the branch nothing
    assert_eq!(db.things("app_feature_auth"), ["one"]);
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "skipped_existing"), 1);

    // a variable pointing somewhere unexpected is a conflict unless --force
    fs::write(auth.join(".env"), format!("DATABASE_URL=\"{}\"\n", db.url("somewhere_else"))).unwrap();
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 3);
    assert_eq!(summary(&r, "conflicts"), 1);
    assert_eq!(env_database(&auth), "somewhere_else");
    let r = run(&["apply", "--force"], &auth, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(env_database(&auth), "app_feature_auth");

    // apply refuses to adopt a database it did not create
    db.admin.batch_execute("CREATE DATABASE \"app_unmanaged\"").unwrap();
    let unmanaged = root.join("app-unmanaged");
    git(&["worktree", "add", "-q", unmanaged.to_str().unwrap(), "-b", "unmanaged"], &repo);
    fs::write(unmanaged.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &unmanaged, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert_eq!(summary(&r, "conflicts"), 1);
    assert_eq!(summary(&r, "rewritten"), 0);
    assert_eq!(env_database(&unmanaged), "app");
    git(&["worktree", "remove", "--force", unmanaged.to_str().unwrap()], &repo);
    db.admin.batch_execute("DROP DATABASE \"app_unmanaged\"").unwrap();

    // template create snapshots the live database
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(db.flags("app_template"), (true, false));
    let snapshot_at = db.meta("app_template")["createdAt"].as_str().unwrap().to_string();
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0);
    assert_eq!(summary(&r, "skipped"), 1);

    // a branch whose fork name is the template's is a conflict about the template, not about a
    // database worktreepg knows nothing of
    let named = root.join("app-template");
    git(&["worktree", "add", "-q", named.to_str().unwrap(), "-b", "template"], &repo);
    fs::write(named.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &named, true);
    assert_eq!(r.code, 3, "{}", r.stderr);
    let conflict = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "conflict").unwrap();
    assert_eq!(conflict["status"], "template");
    git(&["worktree", "remove", "--force", named.to_str().unwrap()], &repo);

    // with nothing connected to the live database, apply still clones it directly, and brings
    // the template up to date while it is at it
    let mut live = db.client("app");
    live.batch_execute("INSERT INTO things VALUES ('two')").unwrap();
    live.close().unwrap();
    db.wait_idle("app");
    let fresh = root.join("app-fresh");
    git(&["worktree", "add", "-q", fresh.to_str().unwrap(), "-b", "fresh"], &repo);
    fs::write(fresh.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &fresh, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "template_refreshed"), 1);
    assert_eq!(db.things("app_fresh"), ["one", "two"]);
    assert_eq!(db.meta("app_fresh")["template"], "app");
    let refresh = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "refresh_template").unwrap();
    assert_eq!(refresh["database"], "app_template");
    assert_eq!(refresh["source"], "app");
    assert_eq!(db.flags("app_template"), (true, false));
    assert_ne!(db.meta("app_template")["createdAt"].as_str().unwrap(), snapshot_at, "template was re-snapshotted");
    assert_eq!(db.meta("app_template")["source"], "app");

    // while the live database is in use, apply falls back to the template and says so
    let busy = root.join("app-busy");
    git(&["worktree", "add", "-q", busy.to_str().unwrap(), "-b", "busy"], &repo);
    fs::write(busy.join(".env"), &source_env).unwrap();
    {
        let mut holder = db.client("app");
        holder.batch_execute("INSERT INTO things VALUES ('three')").unwrap();
        let template_created_at = db.meta("app_template")["createdAt"].as_str().unwrap().to_string();
        let r = run(&["apply", "--dry-run"], &busy, true);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let create = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "create_database").unwrap();
        assert_eq!(create["from"], "app_template");
        assert_eq!(create["status"], "planned");
        assert_eq!(create["origin"], "template");
        assert_eq!(create["snapshot_created_at"], template_created_at);
        assert_eq!(create["snapshot_age"], "a moment");
        assert_eq!(summary(&r, "template_refresh_planned"), 0);
        assert!(!db.exists("app_busy"));
        let r = run(&["apply"], &busy, false);
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stdout.contains("create    app_busy (from app_template, "), "{}", r.stdout);
        assert!(r.stdout.contains(", origin=template)"), "{}", r.stdout);
        assert!(r.stderr.contains("app has 1 open connection, so app_busy is a copy of app_template, taken a moment ago"), "{}", r.stderr);
        assert!(r.stderr.contains("--terminate"), "{}", r.stderr);
        assert_eq!(db.things("app_busy"), ["one", "two"]);
        assert_eq!(db.meta("app_busy")["template"], "app_template");
        let r = run(&["apply", "--recreate"], &busy, true);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let create = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "create_database").unwrap();
        assert_eq!(create["from"], "app_template");
        // The count is reported because no row was masked: this run is a superuser, and
        // pg_stat_activity names the type of every session for it, so the one connection here is
        // the holder and not a worker. two_owners_on_one_cluster is the other half of this,
        // where the session holding the database belongs to a role the run cannot read.
        assert_eq!(create["live_connections"], 1, "{}", r.stderr);
        assert_eq!(create["origin"], "template");
        assert_eq!(create["snapshot_created_at"], template_created_at);
        assert_eq!(summary(&r, "template_refreshed"), 0);

        // template refresh itself still needs the live database free, and keeps the old snapshot when it is not
        let r = run(&["template", "refresh"], &repo, true);
        assert_eq!(r.code, 4, "{}", r.stderr);
        assert!(r.stderr.contains("1 open connection"), "{}", r.stderr);
        assert!(r.stderr.contains("--terminate"), "{}", r.stderr);
        assert_eq!(db.flags("app_template"), (true, false));

        // --terminate closes the connections and clones the live database after all
        let r = run(&["apply", "--recreate", "--terminate"], &busy, true);
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert_eq!(summary(&r, "template_refreshed"), 1);
        assert_eq!(db.things("app_busy"), ["one", "three", "two"]);
        assert_eq!(db.meta("app_busy")["template"], "app");
        assert!(holder.batch_execute("SELECT 1").is_err(), "holder's connection was terminated");
    }
    // --keep-worktree only drops the database
    let r = run(&["remove", "--keep-worktree", busy.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "worktree_removed"), 0);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("app_busy"));
    assert!(git(&["worktree", "list"], &repo).contains("app-busy"), "worktree kept");
    let r = run(&["remove", busy.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "worktree_removed"), 1);
    assert!(!git(&["worktree", "list"], &repo).contains("app-busy"));

    // template refresh replaces the snapshot on demand
    db.wait_idle("app");
    let r = run(&["template", "refresh"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert_eq!(summary(&r, "created"), 1);

    // list shows the template and every fork with its worktree status
    let r = run(&["list"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut names: Vec<&str> = r.json["databases"].as_array().unwrap().iter().map(|d| d["database"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["app_feature_auth", "app_fresh", "app_template"]);
    let fresh_row = r.json["databases"].as_array().unwrap().iter().find(|d| d["database"] == "app_fresh").unwrap();
    assert_eq!(fresh_row["worktree_exists"], true);

    // human-readable output: one line per database and a summary
    let r = run(&["list"], &repo, false);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.lines().any(|l| l.starts_with("template  app_template  from app")), "{}", r.stdout);
    assert!(r.stdout.lines().any(|l| l.starts_with("fork      app_feature_auth  ") && l.ends_with("app-auth")), "{}", r.stdout);
    assert!(r.stdout.lines().any(|l| l == "databases=3"), "{}", r.stdout);
    let r = run(&["list", "--quiet"], &repo, false);
    assert_eq!(r.stdout, "");

    // remove connects to every cluster its directives name before touching anything, so a server
    // it cannot reach leaves both the worktree and the fork alone, and says which state it left
    fs::write(repo.join(".env"), format!("DATABASE_URL=\"{}?sslmode=disable\"\nOTHER=keep\n", unreachable(&live_url))).unwrap();
    let r = run(&["remove", auth.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("cannot connect"), "{}", r.stderr);
    assert!(r.stderr.contains("was left in place and no database was dropped"), "{}", r.stderr);
    assert!(git(&["worktree", "list"], &repo).contains("app-auth"), "worktree survives a failed pre-flight");
    assert!(db.exists("app_feature_auth"), "fork survives a failed pre-flight");
    fs::write(repo.join(".env"), &source_env).unwrap();

    // remove drops the fork and the worktree
    let r = run(&["remove", auth.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "worktree_removed"), 1);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("app_feature_auth"));
    assert!(!git(&["worktree", "list"], &repo).contains("app-auth"));

    // remove --dry-run touches nothing
    let r = run(&["remove", "--dry-run", fresh.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "worktree_removed"), 1);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(db.exists("app_fresh"), "dry-run does not drop anything");
    assert!(git(&["worktree", "list"], &repo).contains("app-fresh"), "dry-run does not remove the worktree");

    // prune drops forks whose worktree was removed with plain git, and never the template
    git(&["worktree", "remove", "--force", fresh.to_str().unwrap()], &repo);
    let r = run(&["prune", "--dry-run"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(db.exists("app_fresh"));
    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("app_fresh"));
    assert!(db.exists("app_template"));

    // template drop removes the snapshot
    let r = run(&["template", "drop"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!db.exists("app_template"));

    // usage errors exit 2; a missing directive exits 4
    assert_eq!(run(&["bogus"], &repo, true).code, 2);
    fs::write(repo.join(".worktreeinclude"), ".env\n").unwrap();
    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 4);
    assert!(r.stderr.contains("no \"# worktreepg\" directive"), "{}", r.stderr);

    db.admin.batch_execute("DROP DATABASE \"app\" WITH (FORCE)").unwrap();
}

/// A repository whose app connects as a runtime role that owns nothing: one directive URL is
/// privileged and the others are not. Every administrative statement has to run as the first.
#[test]
fn mixed_credentials() {
    let Some(admin_url) = test_url("mixed_credentials") else { return };
    let mut db = Db::connect(&admin_url, "mixed");
    db.role("mixed_runtime");
    // so a run that gets as far as a statement fails on ownership, which is what this test is
    // about, and never on the SHOW data_directory behind the --verbose storage note
    db.grant("mixed_runtime", "pg_read_all_settings");
    let owner_url = db.url("mixed");
    let runtime_url = db.url_as("mixed_runtime", "mixed");

    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let repo = root.join("mixed");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\nruntime.env\n.worktreeinclude\nruntime.worktreeinclude\nreversed.worktreeinclude\n").unwrap();
    let include = "# worktreepg: .env DATABASE_URL\n# worktreepg: runtime.env DATABASE_URL\n.env\nruntime.env\n";
    fs::write(repo.join(".worktreeinclude"), include).unwrap();
    // the same database, named only by the role that does not own it
    fs::write(repo.join("runtime.worktreeinclude"), "# worktreepg: runtime.env DATABASE_URL\nruntime.env\n").unwrap();
    // both roles, the wrong one first
    fs::write(
        repo.join("reversed.worktreeinclude"),
        "# worktreepg: runtime.env DATABASE_URL\n# worktreepg: .env DATABASE_URL\nruntime.env\n.env\n",
    )
    .unwrap();
    let owner_env = format!("DATABASE_URL=\"{owner_url}\"\n");
    let runtime_env = format!("DATABASE_URL=\"{runtime_url}\"\n");
    fs::write(repo.join(".env"), &owner_env).unwrap();
    fs::write(repo.join("runtime.env"), &runtime_env).unwrap();
    fs::write(repo.join("README.md"), "mixed\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);

    let work = root.join("mixed-work");
    git(&["worktree", "add", "-q", work.to_str().unwrap(), "-b", "work"], &repo);
    fs::write(work.join(".env"), &owner_env).unwrap();
    fs::write(work.join("runtime.env"), &runtime_env).unwrap();

    // one fork for the one database, and each variable keeps its own credentials
    let r = run(&["apply"], &work, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "matched"), 2);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "rewritten"), 2);
    assert_eq!(fs::read_to_string(work.join(".env")).unwrap(), format!("DATABASE_URL=\"{}\"\n", db.url("mixed_work")));
    assert_eq!(
        fs::read_to_string(work.join("runtime.env")).unwrap(),
        format!("DATABASE_URL=\"{}\"\n", db.url_as("mixed_runtime", "mixed_work"))
    );

    // template create runs once, as the owner
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);

    // the destructive paths: dropping the fork and dropping the snapshot both need the owner
    let r = run(&["apply", "--recreate"], &work, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "skipped_same"), 2);
    let r = run(&["template", "refresh"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert_eq!(summary(&r, "created"), 1);

    // list reports each database once, not once per set of credentials
    let r = run(&["list"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut names: Vec<&str> = r.json["databases"].as_array().unwrap().iter().map(|d| d["database"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["mixed_template", "mixed_work"]);

    // and remove drops the fork once
    let r = run(&["remove", work.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("mixed_work"));

    // with only the runtime role listed there is nothing to fall back on. The error names the
    // statement and the role it ran as, and suggests no ordering, because there is no other URL
    // to put first
    let again = root.join("mixed-again");
    git(&["worktree", "add", "-q", again.to_str().unwrap(), "-b", "again"], &repo);
    fs::write(again.join(".env"), &owner_env).unwrap();
    fs::write(again.join("runtime.env"), &runtime_env).unwrap();
    let r = run(&["apply", "--include", "runtime.worktreeinclude"], &again, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    let failure = message(&r, "error");
    assert!(failure.contains("creating database mixed_again as \"mixed_runtime\""), "{failure}");
    assert!(failure.contains("permission denied"), "{failure}");
    assert!(failure.contains("needs CREATEDB"), "{failure}");
    assert!(!failure.contains("list its URL first"), "{failure}");
    assert!(!db.exists("mixed_again"));

    // list the unprivileged URL first and everything runs as it. The message says what the role
    // lacked and names the other role the directives offer, which is as far as it can go: nothing
    // here knows which of them owns the database
    let reversed = root.join("mixed-reversed");
    git(&["worktree", "add", "-q", reversed.to_str().unwrap(), "-b", "reversed"], &repo);
    fs::write(reversed.join(".env"), &owner_env).unwrap();
    fs::write(reversed.join("runtime.env"), &runtime_env).unwrap();
    let r = run(&["apply", "--include", "reversed.worktreeinclude"], &reversed, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    let failure = message(&r, "error");
    assert!(failure.contains("creating database mixed_reversed as \"mixed_runtime\""), "{failure}");
    assert!(failure.contains("needs CREATEDB"), "{failure}");
    let admin_role: String = db.admin.query_one("SELECT current_user", &[]).unwrap().get(0);
    assert!(failure.contains(&format!("the other URLs for it connect as \"{admin_role}\"")), "{failure}");
    assert!(failure.contains("list its URL first"), "{failure}");
    assert!(!db.exists("mixed_reversed"));

    let r = run(&["template", "drop"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!db.exists("mixed_template"));
    db.admin.batch_execute("DROP DATABASE \"mixed\" WITH (FORCE)").unwrap();
    db.drop_role("mixed_runtime");
}

/// One cluster holding two databases owned by two different roles. Neither role owns both, so no
/// ordering of the directives gives one set of credentials the run of the cluster: each database
/// has to be worked as the role in the first URL that names that database.
#[test]
fn two_owners_on_one_cluster() {
    let Some(admin_url) = test_url("two_owners_on_one_cluster") else { return };
    let mut db = Db::connect(&admin_url, "owners");
    db.owner_of("owners_ra", "owners_alpha");
    db.owner_of("owners_rb", "owners_beta");
    let alpha_url = db.url_as("owners_ra", "owners_alpha");
    let beta_url = db.url_as("owners_rb", "owners_beta");

    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let repo = root.join("owners");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\n.worktreeinclude\n").unwrap();
    fs::write(repo.join(".worktreeinclude"), "# worktreepg: .env ALPHA_URL BETA_URL\n.env\n").unwrap();
    let env = format!("ALPHA_URL=\"{alpha_url}\"\nBETA_URL=\"{beta_url}\"\n");
    fs::write(repo.join(".env"), &env).unwrap();
    fs::write(repo.join("README.md"), "owners\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);

    let work = root.join("owners-work");
    git(&["worktree", "add", "-q", work.to_str().unwrap(), "-b", "two"], &repo);
    fs::write(work.join(".env"), &env).unwrap();

    // both databases are forked, each by its own owner, and both variables are rewritten
    let r = run(&["apply", "--verbose"], &work, false);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("created=2"), "{}", r.stdout);
    assert!(r.stdout.contains("rewritten=2"), "{}", r.stdout);
    assert_eq!(db.owner("owners_alpha_two"), "owners_ra");
    assert_eq!(db.owner("owners_beta_two"), "owners_rb");
    assert_eq!(
        fs::read_to_string(work.join(".env")).unwrap(),
        format!(
            "ALPHA_URL=\"{}\"\nBETA_URL=\"{}\"\n",
            db.url_as("owners_ra", "owners_alpha_two"),
            db.url_as("owners_rb", "owners_beta_two")
        )
    );
    // neither owner can read data_directory, and the storage note is worth less than the command
    assert!(r.stdout.contains("data directory is not readable"), "{}", r.stdout);

    // a snapshot of each, taken as each owner
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 2);
    assert_eq!(db.owner("owners_alpha_template"), "owners_ra");

    // the destructive path: each fork is dropped and re-made by the role that owns its source
    let r = run(&["apply", "--recreate"], &work, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 2);
    assert_eq!(summary(&r, "template_refreshed"), 2);

    let busy = root.join("owners-busy");
    git(&["worktree", "add", "-q", busy.to_str().unwrap(), "-b", "busy"], &repo);
    fs::write(busy.join(".env"), &env).unwrap();

    // a connection held by the role that does the work is counted as what it is: pg_stat_activity
    // gives a role the type of every session whose role it holds the privileges of, which on the
    // ordinary cluster where the app and the admin URL share a role is all of them
    {
        let _app = Client::connect(&db.url_as("owners_ra", "owners_alpha"), NoTls).unwrap();
        let r = run(&["apply", "--dry-run"], &busy, false);
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stderr.contains("owners_alpha has 1 open connection"), "{}", r.stderr);
    }
    db.wait_idle("owners_alpha");

    // a live database in use falls back to its template, and the fallback is reported without a
    // number when the session holding it belongs to a role this one cannot read: backend_type
    // comes back NULL, so what can be counted is an upper bound over client backends and
    // autovacuum workers together rather than a count of the app
    {
        // held as the superuser, so owners_ra is not the role that owns this backend
        let _holder = db.client("owners_alpha");
        let r = run(&["apply", "--dry-run"], &busy, false);
        assert_eq!(r.code, 0, "{}", r.stderr);
        // a dry run predicts the fallback from that bound, and says so rather than reporting it
        assert!(r.stderr.contains("owners_alpha looks to be in use by something this role cannot identify"), "{}", r.stderr);
        assert!(!r.stderr.contains("open connection"), "{}", r.stderr);
        let r = run(&["apply"], &busy, true);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let create = r.json["actions"].as_array().unwrap().iter().find(|a| a["database"] == "owners_alpha_busy").unwrap();
        assert_eq!(create["from"], "owners_alpha_template");
        assert_eq!(create["origin"], "template");
        assert!(create.get("live_connections").is_none(), "{}", r.stdout);
        // and again over the fork it just made, for the message: this one is not a prediction,
        // Postgres refused the live database
        let r = run(&["apply", "--recreate"], &busy, false);
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert!(r.stderr.contains("owners_alpha is in use by something this role cannot identify"), "{}", r.stderr);
    }
    let r = run(&["remove", busy.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 2);

    let r = run(&["list"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut names: Vec<&str> = r.json["databases"].as_array().unwrap().iter().map(|d| d["database"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["owners_alpha_template", "owners_alpha_two", "owners_beta_template", "owners_beta_two"]);
    // the server field is the cluster, so it carries no role name
    assert!(!r.json["databases"][0]["server"].as_str().unwrap().contains('@'), "{}", r.stdout);

    // and remove drops both forks, each on its own owner's connection
    let r = run(&["remove", work.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 2);
    assert!(!db.exists("owners_alpha_two"));
    assert!(!db.exists("owners_beta_two"));

    let r = run(&["template", "drop"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 2);

    for name in ["owners_alpha", "owners_beta", "owners"] {
        db.admin.batch_execute(&format!("DROP DATABASE \"{name}\" WITH (FORCE)")).unwrap();
    }
    db.drop_role("owners_ra");
    db.drop_role("owners_rb");
}

/// A failure on one database no longer costs the run the databases that did fork: their variables
/// are still rewritten, the one that failed keeps naming its live database, and a second run picks
/// up where this one stopped. The failure a connection could have predicted never reaches the
/// loop, and under `--recreate`, where carrying on would put another fork's data at risk, the loop
/// stops at the first failure instead.
#[test]
fn one_database_failing_still_rewrites_the_rest() {
    let Some(admin_url) = test_url("one_database_failing_still_rewrites_the_rest") else { return };
    let mut db = Db::connect(&admin_url, "half");
    db.owner_of("half_ra", "half_alpha");
    db.owner_of("half_rb", "half_beta");
    let alpha_url = db.url_as("half_ra", "half_alpha");
    let beta_url = db.url_as("half_rb", "half_beta");

    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let repo = root.join("half");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\n.worktreeinclude\n").unwrap();
    fs::write(repo.join(".worktreeinclude"), "# worktreepg: .env ALPHA_URL BETA_URL\n.env\n").unwrap();
    let env = format!("ALPHA_URL=\"{alpha_url}\"\nBETA_URL=\"{beta_url}\"\n");
    fs::write(repo.join(".env"), &env).unwrap();
    fs::write(repo.join("README.md"), "half\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);

    // the permission flavour: beta's owner cannot fork it, which no connection can tell in advance
    let work = root.join("half-work");
    git(&["worktree", "add", "-q", work.to_str().unwrap(), "-b", "wt"], &repo);
    fs::write(work.join(".env"), &env).unwrap();
    db.admin.batch_execute("ALTER ROLE \"half_rb\" NOCREATEDB").unwrap();

    let r = run(&["apply"], &work, false);
    assert_eq!(r.code, 4, "an environment error stays one rather than becoming the generic 1: {}", r.stdout);
    assert!(r.stderr.contains("error     half_beta_wt:"), "{}", r.stderr);
    assert!(r.stderr.contains("needs CREATEDB"), "{}", r.stderr);
    // the server's own words, not only worktreepg's description of what it was doing
    assert!(r.stderr.contains("permission denied"), "{}", r.stderr);
    assert!(r.stdout.contains("created=1"), "{}", r.stdout);
    assert!(r.stdout.contains("errors=1"), "{}", r.stdout);
    assert!(r.stdout.contains("rewritten=1"), "{}", r.stdout);
    assert!(db.exists("half_alpha_wt"));
    assert!(!db.exists("half_beta_wt"));
    assert_eq!(
        fs::read_to_string(work.join(".env")).unwrap(),
        format!("ALPHA_URL=\"{}\"\nBETA_URL=\"{beta_url}\"\n", db.url_as("half_ra", "half_alpha_wt")),
        "the fork that was made is reachable, and the one that was not still names its live database"
    );

    // --quiet prints no summary, so stderr is the only stream left for the failure to reach a
    // script on: counting it rather than returning it must not take it off both
    let r = run(&["apply", "--quiet"], &work, false);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert_eq!(r.stdout, "", "{}", r.stdout);
    assert!(r.stderr.contains("error     half_beta_wt:"), "{}", r.stderr);

    // and once the reason is gone a second run finishes the job, adopting the fork already there
    db.admin.batch_execute("ALTER ROLE \"half_rb\" CREATEDB").unwrap();
    let r = run(&["apply"], &work, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(summary(&r, "skipped_existing"), 1);
    assert_eq!(summary(&r, "rewritten"), 1);
    assert_eq!(
        fs::read_to_string(work.join(".env")).unwrap(),
        format!("ALPHA_URL=\"{}\"\nBETA_URL=\"{}\"\n", db.url_as("half_ra", "half_alpha_wt"), db.url_as("half_rb", "half_beta_wt"))
    );

    // --recreate is the flag under which carrying on would cost data, because the fork is dropped
    // to make room for its replacement. half_alpha is in use and has no snapshot to fall back on,
    // so its fork is not dropped at all, and the run stops rather than trying the same on half_beta.
    {
        let _holder = db.client("half_alpha");
        let r = run(&["apply", "--recreate"], &work, true);
        assert_eq!(r.code, 4, "{}", r.stderr);
        assert_eq!(summary(&r, "errors"), 1);
        assert_eq!(summary(&r, "created"), 0, "half_beta was not attempted after half_alpha failed");
        assert!(message(&r, "error").contains("half_alpha"), "{}", r.stdout);
        assert!(db.exists("half_alpha_wt"), "the fork that could not be replaced is still there");
        assert!(db.exists("half_beta_wt"), "and so is the one the run never reached");
    }
    db.wait_idle("half_alpha");

    // the connectivity flavour: a server that will not answer is settled before the first fork.
    // The directives take their URLs from the source worktree, so that is where the dead port goes.
    let down = root.join("half-down");
    git(&["worktree", "add", "-q", down.to_str().unwrap(), "-b", "down"], &repo);
    let down_env = format!("ALPHA_URL=\"{alpha_url}\"\nBETA_URL=\"postgres://half_rb:pw@127.0.0.1:1/half_beta\"\n");
    fs::write(repo.join(".env"), &down_env).unwrap();
    fs::write(down.join(".env"), &down_env).unwrap();

    let r = run(&["apply"], &down, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("nothing was created and no env file was written"), "{}", r.stderr);
    assert!(!db.exists("half_alpha_down"), "the reachable database is not forked either");
    assert_eq!(fs::read_to_string(down.join(".env")).unwrap(), down_env);

    for name in ["half_alpha_wt", "half_beta_wt", "half_alpha", "half_beta", "half"] {
        db.admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")).unwrap();
    }
    db.drop_role("half_ra");
    db.drop_role("half_rb");
}

/// A fork whose source database no directive names any more. Nothing that scans the cluster can
/// reach the owning role through the directives, so the drop is refused; the fork is skipped and
/// the rest of the run finishes, and the message says what brings it back within reach. Where any
/// URL on the cluster does connect as the owning role, the refusal is retried there and nothing is
/// skipped at all. A refusal that is not about ownership is not a skip: no directive puts it
/// right, so it still fails the run.
#[test]
fn a_fork_no_directive_reaches_is_skipped_rather_than_stopping_the_run() {
    let Some(admin_url) = test_url("a_fork_no_directive_reaches_is_skipped_rather_than_stopping_the_run") else { return };
    let mut db = Db::connect(&admin_url, "orphan");
    db.owner_of("orphan_ra", "orphan_alpha");
    db.owner_of("orphan_rb", "orphan_beta");
    db.owner_of("orphan_rb", "orphan_gamma");
    let alpha_url = db.url_as("orphan_ra", "orphan_alpha");
    let beta_url = db.url_as("orphan_rb", "orphan_beta");
    let gamma_url = db.url_as("orphan_rb", "orphan_gamma");

    let root = tempfile::tempdir().unwrap();
    let root: PathBuf = root.path().canonicalize().unwrap();
    let repo = root.join("orphan");
    fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    fs::write(repo.join(".gitignore"), ".env\n.worktreeinclude\n").unwrap();
    let directive = |vars: &str| fs::write(repo.join(".worktreeinclude"), format!("# worktreepg: .env {vars}\n.env\n")).unwrap();
    directive("ALPHA_URL BETA_URL");
    // every URL stays in the env file throughout; what changes is which of them a directive names
    let env = format!("ALPHA_URL=\"{alpha_url}\"\nBETA_URL=\"{beta_url}\"\nGAMMA_URL=\"{gamma_url}\"\n");
    fs::write(repo.join(".env"), &env).unwrap();
    fs::write(repo.join("README.md"), "orphan\n").unwrap();
    git(&["add", "."], &repo);
    git(&["commit", "-q", "-m", "init"], &repo);

    let gone = root.join("orphan-gone");
    git(&["worktree", "add", "-q", gone.to_str().unwrap(), "-b", "gone"], &repo);
    fs::write(gone.join(".env"), &env).unwrap();
    let r = run(&["apply"], &gone, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 2);
    assert_eq!(db.owner("orphan_beta_gone"), "orphan_rb");

    // the directive naming orphan_beta is removed, so the only credentials left on this cluster
    // are orphan_ra's, and orphan_ra owns neither orphan_beta nor its fork
    directive("ALPHA_URL");

    // a dry run runs no statement, so it asks the server the question the drop would ask rather
    // than promising a plan it cannot carry out: the skip it predicts is the one the real run
    // reports, down to the exit code
    let r = run(&["remove", gone.to_str().unwrap(), "--dry-run"], &repo, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert_eq!(summary(&r, "skipped"), 1);
    let skip = r.json["actions"].as_array().unwrap().iter().find(|a| a["op"] == "skip").unwrap();
    assert_eq!(skip["database"], "orphan_beta_gone");
    assert_eq!(skip["status"], "not_owner");
    assert_eq!(skip["owner"], "orphan_rb");
    assert_eq!(skip["predicted"], true);
    assert!(gone.is_dir());

    // the real run drops what it can, skips what it cannot, and exits 4
    let r = run(&["remove", gone.to_str().unwrap()], &repo, false);
    assert_eq!(r.code, 4, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("dropped=1"), "{}", r.stdout);
    assert!(r.stdout.contains("skipped=1"), "{}", r.stdout);
    assert!(r.stdout.contains("skip      orphan_beta_gone"), "{}", r.stdout);
    assert!(r.stdout.contains("must be owner of database orphan_beta_gone"), "{}", r.stdout);
    assert!(r.stdout.contains("connects as \"orphan_rb\""), "{}", r.stdout);
    assert!(r.stdout.contains("such as the one for \"orphan_beta\""), "{}", r.stdout);
    assert!(!gone.exists());
    assert!(!db.exists("orphan_alpha_gone"));
    assert!(db.exists("orphan_beta_gone"));

    // and prune says the same thing rather than failing, so the fork is not stuck behind a hard
    // error on every later run. --quiet takes the line off stdout, and a run that leaves work
    // undone still has to say which fork it was, so the same line goes to stderr
    let r = run(&["prune", "--quiet"], &repo, false);
    assert_eq!(r.code, 4, "{}\n{}", r.stdout, r.stderr);
    assert_eq!(r.stdout, "", "{}", r.stdout);
    assert!(r.stderr.contains("skip      orphan_beta_gone"), "{}", r.stderr);

    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 0);
    assert_eq!(summary(&r, "skipped"), 1);
    let skip = r.json["actions"].as_array().unwrap().iter().find(|a| a["database"] == "orphan_beta_gone").unwrap();
    assert_eq!(skip["status"], "not_owner");
    assert_eq!(skip["source"], "orphan_beta");
    assert_eq!(skip["owner"], "orphan_rb");
    assert_eq!(skip["role"], "orphan_ra");
    assert_eq!(skip["predicted"], false);

    // the remedy the message names, taken
    directive("ALPHA_URL BETA_URL");
    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert_eq!(summary(&r, "skipped"), 0);
    assert!(!db.exists("orphan_beta_gone"));

    // orphan_rb owns orphan_gamma as well, so a directive naming that one reaches orphan_beta's
    // fork too: the refusal the cluster's own URL meets is retried as the role that owns it
    directive("ALPHA_URL BETA_URL GAMMA_URL");
    let wide = root.join("orphan-wide");
    git(&["worktree", "add", "-q", wide.to_str().unwrap(), "-b", "wide"], &repo);
    fs::write(wide.join(".env"), &env).unwrap();
    let r = run(&["apply"], &wide, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 3);

    directive("ALPHA_URL GAMMA_URL");
    let r = run(&["remove", wide.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 3);
    assert_eq!(summary(&r, "skipped"), 0);
    assert!(!db.exists("orphan_beta_wide"));

    // the same retry where the directive naming the source is the one that cannot drop the fork:
    // BETA_URL is rewritten to connect as orphan_ra, which owns neither orphan_beta nor its fork,
    // and GAMMA_URL is still orphan_rb's
    directive("ALPHA_URL BETA_URL GAMMA_URL");
    let mixed = root.join("orphan-mixed");
    git(&["worktree", "add", "-q", mixed.to_str().unwrap(), "-b", "mixed"], &repo);
    fs::write(mixed.join(".env"), &env).unwrap();
    let r = run(&["apply"], &mixed, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 3);
    fs::write(repo.join(".env"), env.replace(&beta_url, &db.url_as("orphan_ra", "orphan_beta"))).unwrap();
    let r = run(&["remove", mixed.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 3);
    assert_eq!(summary(&r, "skipped"), 0);
    assert!(!db.exists("orphan_beta_mixed"));

    // a refusal that is not about ownership: orphan_rb owns this fork, and the WITH (FORCE) drop
    // is refused over a superuser's backend it may not signal. No directive changes that, so the
    // run fails on it rather than reporting a skip whose remedy would not work
    directive("GAMMA_URL");
    let held = root.join("orphan-held");
    git(&["worktree", "add", "-q", held.to_str().unwrap(), "-b", "held"], &repo);
    fs::write(held.join(".env"), &env).unwrap();
    let r = run(&["apply"], &held, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(db.owner("orphan_gamma_held"), "orphan_rb");
    {
        let _holder = db.client("orphan_gamma_held");
        let r = run(&["remove", held.to_str().unwrap()], &repo, false);
        assert_eq!(r.code, 4, "{}\n{}", r.stdout, r.stderr);
        assert!(!r.stdout.contains("skip      "), "{}", r.stdout);
        assert!(r.stderr.contains("dropping database orphan_gamma_held"), "{}", r.stderr);
        assert!(r.stderr.contains("terminate"), "{}", r.stderr);
        assert!(r.stderr.contains("pg_signal_backend"), "{}", r.stderr);
        assert!(db.exists("orphan_gamma_held"));
    }

    // and once nothing is attached, prune drops the fork the failed run left behind
    db.wait_idle("orphan_gamma_held");
    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("orphan_gamma_held"));

    for name in ["orphan_alpha", "orphan_beta", "orphan_gamma", "orphan"] {
        db.admin.batch_execute(&format!("DROP DATABASE \"{name}\" WITH (FORCE)")).unwrap();
    }
    db.drop_role("orphan_ra");
    db.drop_role("orphan_rb");
}
