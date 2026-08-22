//! End-to-end: the real binary against a real git repository with worktrees and a real
//! Postgres cluster. Needs `WORKTREE_PG_TEST_URL` pointing at a superuser on a maintenance
//! database (see scripts/test-db.sh); skips otherwise.

use postgres::{Client, NoTls};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use worktreepg::pgurl::with_database;

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
    fn connect(url: &str) -> Self {
        let mut admin = Client::connect(url, NoTls).expect("connect to WORKTREE_PG_TEST_URL");
        for name in ["app", "app_feature_auth", "app_stale", "app_unmanaged", "app_template"] {
            admin.batch_execute(&format!("ALTER DATABASE \"{name}\" WITH IS_TEMPLATE false")).ok();
            admin.batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")).unwrap();
        }
        admin.batch_execute("CREATE DATABASE \"app\"").unwrap();
        Self { admin, admin_url: url.to_string() }
    }

    fn url(&self, database: &str) -> String {
        with_database(&self.admin_url, database)
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

#[test]
fn end_to_end() {
    let Ok(admin_url) = std::env::var("WORKTREE_PG_TEST_URL") else {
        eprintln!("skipping end_to_end: set WORKTREE_PG_TEST_URL (see scripts/test-db.sh)");
        return;
    };
    let mut db = Db::connect(&admin_url);
    let live_url = db.url("app");
    {
        let mut live = db.client("app");
        live.batch_execute("CREATE TABLE things (name text PRIMARY KEY); INSERT INTO things VALUES ('one')").unwrap();
    }

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

    // apply is idempotent
    let r = run(&["apply"], &auth, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "skipped_existing"), 1);
    assert_eq!(summary(&r, "skipped_same"), 1);

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

    // template create snapshots the live database; forks then come from it
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(db.flags("app_template"), (true, false));
    let r = run(&["template", "create"], &repo, true);
    assert_eq!(r.code, 0);
    assert_eq!(summary(&r, "skipped"), 1);
    db.client("app").batch_execute("INSERT INTO things VALUES ('two')").unwrap();
    let stale = root.join("app-stale");
    git(&["worktree", "add", "-q", stale.to_str().unwrap(), "-b", "stale"], &repo);
    fs::write(stale.join(".env"), &source_env).unwrap();
    let r = run(&["apply"], &stale, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(db.things("app_stale"), ["one"]);
    assert_eq!(db.meta("app_stale")["template"], "app_template");

    // template refresh rebuilds the snapshot; apply --recreate re-forks from it
    let r = run(&["template", "refresh"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert_eq!(summary(&r, "created"), 1);
    let r = run(&["apply", "--recreate"], &stale, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "created"), 1);
    assert_eq!(db.things("app_stale"), ["one", "two"]);

    // copying a database with open connections fails clearly unless --terminate is given
    {
        let _holder = db.client("app");
        let r = run(&["template", "refresh"], &repo, true);
        assert_eq!(r.code, 4, "{}", r.stderr);
        assert!(r.stderr.contains("1 open connection"), "{}", r.stderr);
        assert!(r.stderr.contains("--terminate"), "{}", r.stderr);
        let r = run(&["template", "refresh", "--terminate"], &repo, true);
        assert_eq!(r.code, 0, "{}", r.stderr);
    }

    // list shows the template and every fork with its worktree status
    let r = run(&["list"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut names: Vec<&str> = r.json["databases"].as_array().unwrap().iter().map(|d| d["database"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["app_feature_auth", "app_stale", "app_template"]);
    let stale_row = r.json["databases"].as_array().unwrap().iter().find(|d| d["database"] == "app_stale").unwrap();
    assert_eq!(stale_row["worktree_exists"], true);

    // human-readable output: one line per database and a summary
    let r = run(&["list"], &repo, false);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.lines().any(|l| l.starts_with("template  app_template  from app")), "{}", r.stdout);
    assert!(r.stdout.lines().any(|l| l.starts_with("fork      app_feature_auth  ") && l.ends_with("app-auth")), "{}", r.stdout);
    assert!(r.stdout.lines().any(|l| l == "databases=3"), "{}", r.stdout);
    let r = run(&["list", "--quiet"], &repo, false);
    assert_eq!(r.stdout, "");

    // remove drops the fork and the worktree
    let r = run(&["remove", auth.to_str().unwrap()], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "worktree_removed"), 1);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("app_feature_auth"));
    assert!(!git(&["worktree", "list"], &repo).contains("app-auth"));

    // prune drops forks whose worktree was removed with plain git, and never the template
    git(&["worktree", "remove", "--force", stale.to_str().unwrap()], &repo);
    let r = run(&["prune", "--dry-run"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(db.exists("app_stale"));
    let r = run(&["prune"], &repo, true);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(summary(&r, "dropped"), 1);
    assert!(!db.exists("app_stale"));
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
