# worktreepg

Fork a Postgres database per git worktree.

`worktreepg` reads a `# worktreepg` comment out of your [`.worktreeinclude`](https://github.com/satococoa/git-worktreeinclude) file, copies your development database under a name derived from the worktree's branch, and rewrites the connection string in the worktree's env file to point at the copy. When the worktree goes away, so does the database.

It pairs with [portless](https://github.com/vercel-labs/portless), which already gives each worktree its own `https://<branch>.<app>.localhost`. With both, a new worktree gets its own URL and its own data with no config changes.

## Requirements

- Postgres 13 or newer (for `DROP DATABASE ... WITH (FORCE)`). Tested against Postgres 18.
- The connecting role needs `CREATEDB`, and must own the development database or be a superuser, which is the normal situation for a local dev cluster.
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

Then take a snapshot of your development database:

```sh
git worktreepg template create
```

This creates `<database>_template`, flagged as a template database that nothing can connect to. Forks are cloned from it, so creating a worktree never needs your dev server to be stopped. Refresh it whenever you want new worktrees to start from current data:

```sh
git worktreepg template refresh
```

The snapshot is optional. Without one, `apply` clones the live database directly, which only works while nothing is connected to it.

## Workflow

```sh
git worktree add ../app-auth -b feature/auth
git -C ../app-auth worktreeinclude apply
git -C ../app-auth worktreepg apply
cd ../app-auth && portless
```

`apply` creates `app_feature_auth` (for a database named `app`) and rewrites `DATABASE_URL` in `../app-auth/.env` to point at it. Everything else in the URL, and everything else in the file, is left as it was. Running it again is a no-op.

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

- `apply --recreate` drops the worktree's fork and clones it again, which is how you pick up a refreshed template.
- `apply --force` rewrites the env variable even when it points at some database other than the source or the fork. Without it that is reported as a conflict (exit code 3), the same way `git worktreeinclude` treats a differing file.
- `--terminate` closes open connections to the live database before copying it. Postgres refuses to use a database as a template while anyone is connected, and dev servers keep pools open. Nothing is terminated unless you pass the flag; the error tells you how many connections are in the way.
- `remove` runs `git worktree remove` first so git's own checks (uncommitted changes, locks) run before anything is dropped. `--force` is passed through to git. `--keep-worktree` only drops the databases.
- `list --all` includes databases created for other repositories on the same cluster.

## How it works

- The fork is named `<database>_<branch>`, with the branch lowercased and anything outside `[a-z0-9]` turned into `_`, so it never needs quoting: `feature/auth` becomes `app_feature_auth`. The full branch name is used rather than the last segment, so `feature/auth` and `bugfix/auth` do not share a database. On a detached HEAD the worktree directory name is used. `--worktree-name` overrides it.
- Every database worktreepg creates carries a `COMMENT ON DATABASE` starting with `worktreepg ` followed by JSON: the repository (its common `.git` directory), the worktree path, the branch, and where it was copied from. `remove`, `prune`, and `list` find databases through that comment, so nothing is dropped unless worktreepg created it for this repository. `psql`'s `\l+` shows the comment.
- On Postgres 18 and newer, forks and templates are made with `STRATEGY = FILE_COPY` and `file_copy_method = clone`, so the kernel copies the files with `copy_file_range()` (`copyfile` on macOS). On a copy-on-write filesystem (btrfs, bcachefs, APFS, XFS with `reflink=1`, ZFS with block cloning) a fork then shares its blocks with the template and costs no disk until it diverges. Measured on btrfs with an 89 MB database: the clone had 0 B of exclusive extents, where `WAL_LOG` and a plain file copy each wrote a full 88 MiB. The price is the two checkpoints `FILE_COPY` forces, about a second, regardless of size. On filesystems that cannot share blocks the kernel does an ordinary copy, so nothing breaks; `--verbose` reports the copy method and, when the server is local and its data directory is visible, the filesystem it sits on. Inside a container the data directory is not visible from the host, so it says so rather than guessing. Before Postgres 18 the default `WAL_LOG` strategy is used.
- The template is created with `CREATE DATABASE ... TEMPLATE <database>`, then `ALTER DATABASE ... IS_TEMPLATE true ALLOW_CONNECTIONS false`. `IS_TEMPLATE` lets any role with `CREATEDB` clone it and makes a plain `DROP DATABASE` refuse; `ALLOW_CONNECTIONS false` means it can never be "in use" when a fork is being made.
- Administrative statements run over a connection to `postgres` (falling back to `template1`) using the credentials from the env file, so the development database itself is never held open by worktreepg.
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

The end-to-end test drives the built binary against a temporary git repository with real worktrees and a real cluster, creating and dropping databases named `app`, `app_*`. Point `WORKTREE_PG_TEST_URL` at a superuser on any cluster you do not mind that happening to. CI runs it against the `postgres:18` image.

## License

MIT
