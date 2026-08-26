# worktreepg

Fork a Postgres database per git worktree.

`worktreepg` reads a `# worktreepg` comment out of your [`.worktreeinclude`](https://github.com/satococoa/git-worktreeinclude) file, copies your development database under a name derived from the worktree's branch, and rewrites the connection string in the worktree's env file to point at the copy. When the worktree goes away, so does the database.

It pairs with [portless](https://github.com/vercel-labs/portless), which already gives each worktree its own `https://<branch>.<app>.localhost`. With both, a new worktree gets its own URL and its own data with no config changes.

## Requirements

- Postgres 13 or newer (for `DROP DATABASE ... WITH (FORCE)`). Tested against Postgres 18.
- The connecting role needs `CREATEDB`, and must own the development database or be a superuser, which is the normal situation for a local dev cluster. Only one of the URLs naming a database has to: everything done to that database runs as the role in the first directive URL that names it.
- `--terminate` closes connections that belong to other roles, which owning the database does not allow. That needs membership in `pg_signal_backend`, or a superuser.
- `pg_read_all_stats` is optional. `pg_stat_activity` tells a role what a session is only where it holds that session's role's privileges, so a session belonging to another role could be your app or could be an autovacuum worker: worktreepg reports the live database as in use without saying by how many connections. The single-role setup, where the app connects as the role the directive URL names, is unaffected. Which database `apply` copies does not change either way, because Postgres decides that and not the count, but a worker parked on the live database makes `template refresh` refuse where a superuser would have refreshed, and `apply --dry-run` name the template where `apply` copies the live database.
- `git` on `PATH`.
- Connections are made without TLS. Local clusters are the target; a server that insists on SSL will reject the admin connection.

## Install

One binary, `git-worktreepg`, with no runtime dependencies. Git finds it on `PATH`, so it is run as `git worktreepg`.

Prebuilt binaries for Linux (x86_64, arm64), macOS (Intel, Apple silicon), and Windows are on the [releases page](https://github.com/itsjamie/worktreepg/releases), with installers:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/itsjamie/worktreepg/releases/latest/download/worktreepg-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/itsjamie/worktreepg/releases/latest/download/worktreepg-installer.ps1 | iex"
```

Or from source:

```sh
cargo install --git https://github.com/itsjamie/worktreepg
```

`git worktreepg -h` and `git worktreepg apply --help` print help. `git worktreepg --help` does not: git turns `<command> --help` into a man page lookup before the binary ever runs.

## Setup

Add a directive to `.worktreeinclude` in your main worktree. It is a comment, so `git worktreeinclude` ignores it:

```gitignore
# worktreepg: .env DATABASE_URL
.env
```

The first token is the env file, relative to the worktree root. Everything after it is a variable in that file holding a `postgres://` URL. Both default, so `# worktreepg` on its own means `.env DATABASE_URL`. More than one directive is fine:

```gitignore
# worktreepg: apps/api/.env DATABASE_URL DIRECT_URL
# worktreepg: packages/db/.env DATABASE_URL
```

Variables naming the same database with different credentials are still one database, forked once, and each variable is rewritten keeping its own credentials. Everything done to a database runs as the role in the first directive URL that names that database, so when your app connects as a runtime role that owns nothing, put the URL that owns the database first:

```gitignore
# worktreepg: .env DATABASE_URL                       <- owns the database
# worktreepg: apps/api/.env DATABASE_URL AUDIT_URL    <- runtime role, cannot create or drop
```

That is enough when your dev server is stopped: `apply` clones the live database as it is at that moment, the way `git worktree add` branches from the current commit. Postgres refuses to copy a database that anything is connected to, though, and dev servers keep connection pools open. For that case, take a snapshot once:

```sh
git worktreepg template create
```

This creates `<database>_template`, flagged as a template database that nothing can connect to. While the live database is in use, forks are cloned from the snapshot instead, and `apply` says so and how old it is. Whenever a fork does come from the live database, the snapshot is replaced with a copy of that fork, so it is as current as the last worktree you created. `git worktreepg template refresh` replaces it on demand.

## Workflow

```sh
git worktree add ../app-auth -b feature/auth
git -C ../app-auth worktreeinclude apply
git -C ../app-auth worktreepg apply
cd ../app-auth && portless
```

`apply` creates `app_feature_auth` (for a database named `app`) and rewrites `DATABASE_URL` in `../app-auth/.env` to point at it. Everything else in the URL, and everything else in the file, is left as it was. Running it again is a no-op.

```
create    app_feature_auth (from app, cloned, origin=live)
refresh   app_template (from app, cloned)
rewrite   .env DATABASE_URL -> app_feature_auth
```

With the dev server running, the first line reads `from app_template, cloned, origin=template` instead, and a warning on stderr gives the snapshot's age and what is holding the live database, with a count of the open connections where the role can see one. `apply --recreate --terminate` closes those connections and clones the live database after all.

The order matters: `git worktreeinclude apply` is what puts `.env` in the new worktree, and worktreepg only edits env files it finds there. If it created the file itself, git-worktreeinclude would later overwrite it (or report a conflict). Running `apply` before the file exists stops with exit code 4 and nothing is created.

To remove the worktree and its database in one step:

```sh
git worktreepg remove ../app-auth
```

If you removed the worktree with plain `git worktree remove`, drop the orphaned databases afterwards:

```sh
git worktreepg prune
```

`list` shows the template and every fork, and flags forks whose worktree is gone.

## Commands

```
git worktreepg apply [--worktree-name <name>] [--recreate] [--terminate] [--dry-run] [--force]
git worktreepg remove [<path>] [--keep-worktree] [--dry-run] [--force]
git worktreepg prune [--dry-run]
git worktreepg list [--all]
git worktreepg template create|refresh|drop [--terminate] [--dry-run] [--force]
```

All commands accept `--from auto|<path>`, `--include <path>`, `--json`, `--quiet`, and `--verbose`, with the same meaning as in `git-worktreeinclude`. `--json` emits one object on stdout with a `summary` and an `actions` list; file contents and connection strings are never printed.

- `apply --recreate` drops the worktree's fork and clones it again from current data.
- `apply --force` rewrites the env variable even when it points at some database other than the source or the fork. Without it that is reported as a conflict (exit code 3), the same way `git worktreeinclude` treats a differing file. The conflict is settled before anything is created, so one such variable stops the whole run: nothing is forked, and nothing is rewritten, in that file or any other.
- `apply` connects to every database its directives name before the first `CREATE DATABASE`, so a server that is down or a password that is wrong stops the run having created nothing; it says so, with `nothing was created and no env file was written`. A directive pointing at a cluster that no longer exists blocks the whole run for that reason, the reachable clusters included, until the directive or the env file is fixed. Past the connections, a database that cannot be forked for a reason no connection could predict (a role without `CREATEDB`, say) is reported on stderr, counted under `errors`, and the run carries on, so the databases that did fork get their variables rewritten rather than being left behind pointing at nothing. The exit code is the one that failure would have exited with on its own. A database that failed having created nothing keeps its variables naming its live database, and a second `apply` picks up where the first stopped. A failure after the fork was created leaves a fork no env file names: the next `apply` adopts it, unless what failed was the comment identifying it, which is a conflict. Under `--recreate` the fork is dropped only once the live database looks copyable, so a run with nothing to clone keeps the fork it had; a `--recreate` failure past that point stops the run rather than risking the fork of every database left.
- `--terminate` closes open connections to the live database before copying it. Postgres refuses to use a database as a template while anyone is connected, and dev servers keep pools open. Nothing is terminated unless you pass the flag. Without it, `apply` falls back to the snapshot, and `template create|refresh` stop with an error that says what is in the way.
- `remove` connects to every database its directives name before touching anything, so a server that is down stops the command before the worktree is gone; it says so, with `<path> was left in place and no database was dropped`. A directive pointing at a cluster that no longer exists blocks `remove` for that reason until the directive or the env file is fixed. Past the connections, it runs `git worktree remove` first so git's own checks (uncommitted changes, locks) run before any database is dropped. `--force` is passed through to git. `--keep-worktree` only drops the databases. A fork none of the roles the directives offer owns is skipped rather than failing the command: a fork nothing can drop fails every later run the same way, which leaves it unreachable by any command, so the run finishes the work it can do and reports the one piece it could not. The drop Postgres refused is reported with the role that owns the fork, the other forks are still dropped, and `remove` exits 4. A `--dry-run` predicts the same skip, reports it the same way, and exits the same way; it asks the server whether the role could drop the fork rather than running the drop, so a role that turns out to be a superuser or a member of the owning role is not reported at all. Only a refusal for want of ownership is skipped: a `WITH (FORCE)` drop refused over a backend the role may not signal (the app is still connected to the fork) fails the command, since no directive puts that right.
- `prune` drops every fork whose worktree git no longer lists, and is the way back for a fork `remove` had to skip: list a directive URL that connects as the role owning it, usually the URL for its source database, and run it again. A fork it still cannot drop is skipped the same way, so the forks after it are not held up by it, and `prune` exits 4. `--quiet` keeps the skip, on stderr, since a run that leaves work undone has to say which fork it was.
- `list --all` includes databases created for other repositories on the same cluster.

## How it works

- The fork is named `<database>_<branch>`, with the branch lowercased and anything outside `[a-z0-9]` turned into `_`, so it never needs quoting: `feature/auth` becomes `app_feature_auth`. The full branch name is used rather than the last segment, so `feature/auth` and `bugfix/auth` do not share a database. Names that still collide (`feature/auth-v2` and `feature/auth.v2` both become `app_feature_auth_v2`, as do two branches that agree up to the 63-byte identifier limit) are a conflict naming the worktree that already owns the fork, with or without `--recreate`: rename one branch, or pass `--worktree-name`. A fork whose recorded worktree has moved or been removed hits the same conflict, because the recorded path is only compared and never followed. Moving the worktree back to the recorded path keeps the fork; `prune` drops forks whose worktree git no longer lists, data and all, and the next `apply` makes a fresh one. On a detached HEAD the worktree directory name is used. `--worktree-name` overrides it.
- `apply` tries `CREATE DATABASE ... TEMPLATE <database>` first. Postgres checks for other backends before it copies anything, so when the live database is in use nothing has been created yet and the fork is cloned from `<database>_template` instead. When the fork did come from the live database and a snapshot exists, the snapshot is dropped and recreated as a copy of the fork rather than of the live database: nothing is connected to the fork yet, so that cannot fail the way a second copy of the live database could if the app reconnected in between.
- Every database worktreepg creates carries a `COMMENT ON DATABASE` starting with `worktreepg ` followed by JSON: the repository (its common `.git` directory), the worktree path, the branch, and where it was copied from. `remove`, `prune`, and `list` find databases through that comment, so nothing is dropped unless worktreepg created it for this repository. `psql`'s `\l+` shows the comment.
- On Postgres 18 and newer, forks and templates are made with `STRATEGY = FILE_COPY` and `file_copy_method = clone`, so the kernel copies the files with `copy_file_range()` (`copyfile` on macOS). On a copy-on-write filesystem (btrfs, bcachefs, APFS, XFS with `reflink=1`, ZFS with block cloning) a fork then shares its blocks with the template and costs no disk until it diverges. Measured on btrfs with an 89 MB database: the clone had 0 B of exclusive extents, where `WAL_LOG` and a plain file copy each wrote a full 88 MiB. The price is the two checkpoints `FILE_COPY` forces, about a second, regardless of size. On filesystems that cannot share blocks the kernel does an ordinary copy, so nothing breaks; `--verbose` reports the copy method and, when the server is local and its data directory is visible, the filesystem it sits on. Inside a container the data directory is not visible from the host, so it says so rather than guessing. Finding it at all takes `SHOW data_directory`, which needs superuser or `pg_read_all_settings`; a role that has neither gets a line saying so, since the note is a detail and not a result. Before Postgres 18 the default `WAL_LOG` strategy is used.
- The template is created with `CREATE DATABASE ... TEMPLATE <database>`, then `ALTER DATABASE ... IS_TEMPLATE true ALLOW_CONNECTIONS false`. `IS_TEMPLATE` lets any role with `CREATEDB` clone it and makes a plain `DROP DATABASE` refuse; `ALLOW_CONNECTIONS false` means it can never be "in use" when a fork is being made. `template refresh` checks for connections before dropping the old snapshot, so a refresh that runs into a connected app leaves the snapshot it had.
- Administrative statements run over a connection to `postgres` (falling back to `template1`), so the development database itself is never held open by worktreepg. Each database on a cluster (`host:port`) is forked, snapshotted, or dropped once however many variables point at it, as the role in the first directive URL that names it. Connections are pooled per role, so a cluster holding two databases with different owners gets one connection each and still does each piece of work once. Scans that span a cluster (`list`, and finding a repository's forks for `remove` and `prune`) go over the first URL that named the cluster, and anything they turn up that has to be dropped is dropped on its own database's connection. Where a fork's source database is no longer named by any directive there is no such connection, so the cluster's own URL runs the drop. A drop refused for want of ownership is tried once more on the first URL on the cluster that connects as the role owning the fork, read from `pg_database`: that reaches a role owning two databases where only one of them is still named, and a source database whose own URL connects as some other role. A cluster is identified by the host as it is written in the URL, so `localhost` and `127.0.0.1` count as two even when they are one server, and every scan that spans a cluster then runs once per spelling over the same databases. Nothing is forked or dropped twice, but `list` prints each row twice and `prune`'s `forks` and `kept` counts, any `skipped` count, and the `dropped` count of any `--dry-run`, double with it. Spell the host the same way in every URL. A statement Postgres refuses for want of privileges reports the role it ran as, which is not necessarily the role in the variable being applied, and names the other roles the directives offer for that database.
- Exit codes match `git-worktreeinclude`: `0` success, `1` internal error, `2` usage error, `3` conflict, `4` environment error (not a git repository, cannot connect, database in use, env file not there yet, no directive).

## Copy-on-write on macOS

Postgres running directly on the Mac (Postgres.app, Homebrew) sits on APFS, which clones files, so forks cost no disk. Postgres inside Docker Desktop does not: named volumes live on an ext4 disk image inside Docker's Linux VM, and APFS never sees the individual files.

If you want to keep Docker Compose for everything else and still get free forks, the options in order of how much I would recommend them:

1. Run only Postgres natively and point the Compose services at it through `host.docker.internal`. This is also the fastest Postgres you can have on a Mac, and the tool can see the data directory and tell you what it is on.
2. Use `podman machine` and `podman compose` instead of Docker Desktop. The machine image is Fedora CoreOS, whose root filesystem is XFS with reflink enabled (the `mkfs.xfs` default), so `copy_file_range()` inside the VM shares blocks, including for named volumes. The data directory is not visible from the host, so worktreepg reports that rather than confirming the sharing; check it yourself once with `podman exec <container> cp --reflink=always /some/file /tmp/x`, which fails on a filesystem that cannot share blocks.
3. Lima or Colima with an extra data disk formatted as btrfs or XFS, mounted where the Docker volumes live. Works, but you are assembling it yourself.

Bind-mounting `PGDATA` from APFS into the container is not one of the options: the virtiofs layer does not promise to turn `copy_file_range()` into a clone, and Postgres on a bind-mounted volume in Docker Desktop is slow and has had fsync problems.

## Releasing

Releases are built by [cargo-dist](https://github.com/axodotdev/cargo-dist): bump `version` in `Cargo.toml`, then push a `v*` tag. `.github/workflows/release.yml` builds every target and attaches the archives and installers to a GitHub release. `dist plan` shows what a release will contain; `dist build` produces the host platform's artifacts under `target/distrib/`.

## Development

```sh
cargo test                         # unit tests only
eval "$(scripts/test-db.sh)"       # starts postgres:18 in podman (or docker) and exports WORKTREE_PG_TEST_URL
cargo test                         # now also runs the end-to-end test
scripts/test-db.sh stop
```

The end-to-end tests drive the built binary against temporary git repositories with real worktrees and a real cluster, creating and dropping databases named `app`, `app_*`, `mixed`, `mixed_*`, `orphan`, `orphan_*`, `owners`, `owners_*`, and login roles named `mixed_runtime`, `orphan_ra`, `orphan_rb`, `owners_ra`, and `owners_rb`. Point `WORKTREE_PG_TEST_URL` at a superuser on any cluster you do not mind that happening to. CI runs it against the `postgres:18` image.

## License

MIT
