# worktreepg

Fork a Postgres database per git worktree.

`worktreepg` reads a comment from [`.worktreeinclude`](https://github.com/satococoa/git-worktreeinclude), copies your development database under a branch-derived name, and rewrites the worktree's env file to use that copy. Removing the worktree can remove its database too.

It pairs well with [portless](https://github.com/vercel-labs/portless): each worktree gets its own URL and data without application config changes.

## Requirements

- Postgres 13 or newer; Postgres 18 enables copy-on-write clones where the filesystem supports them.
- `git` on `PATH`.
- A local Postgres role with `CREATEDB` that owns the development database, or a superuser.
- No TLS: remote clusters that require SSL are not supported.

## Install

Prebuilt binaries for Linux, macOS, and Windows are on the [releases page](https://github.com/itsjamie/worktreepg/releases).

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/itsjamie/worktreepg/releases/latest/download/worktreepg-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/itsjamie/worktreepg/releases/latest/download/worktreepg-installer.ps1 | iex"
```

Or install from source:

```sh
cargo install --git https://github.com/itsjamie/worktreepg
```

## Setup

Add a directive to `.worktreeinclude` in the main worktree. It is a comment, so `git worktreeinclude` ignores it:

```gitignore
# worktreepg: .env DATABASE_URL
.env
```

The first token is an env file relative to the worktree root; the remaining tokens are variables containing `postgres://` URLs. `# worktreepg` alone defaults to `.env DATABASE_URL`. Multiple files and variables are supported:

```gitignore
# worktreepg: apps/api/.env DATABASE_URL DIRECT_URL
# worktreepg: packages/db/.env DATABASE_URL
```

If several URLs name the same database, it is copied once and each URL keeps its own credentials. Put an owner URL before any runtime-role URLs: database operations use the first role that names the database.

Create a snapshot if the development database is normally in use:

```sh
git worktreepg template create
```

`apply` copies the live database when it is idle and falls back to `<database>_template` when it is busy. Refresh the snapshot with `git worktreepg template refresh`.

## Workflow

```sh
git worktree add ../app-auth -b feature/auth
git -C ../app-auth worktreeinclude apply
git -C ../app-auth worktreepg apply
```

For a database named `app`, this creates `app_feature_auth` and rewrites the configured variables in `../app-auth`. Run `worktreeinclude apply` first: worktreepg only edits env files already present in the new worktree. Re-running `apply` is a no-op.

Remove the worktree and its databases together:

```sh
git worktreepg remove ../app-auth
```

If the worktree was removed with plain Git, clean up its databases with `git worktreepg prune`.

## Sharing a fork across repositories

Two repositories that point at one development database and run `apply --worktree-name` with the same name, such as the repositories of one [workerbox](https://github.com/itsjamie/workerbox) worker, want the same fork. By default the second is refused. To share the fork instead, name one group in each repository's directive:

```gitignore
# worktreepg: .env DATABASE_URL share=shop
```

The second `apply` joins the fork the first created. Both directives must name the same group: a fork created without a group cannot be joined, and a directive without one cannot join. `remove` and `prune` in one repository release that repository's worktree, and the fork is dropped when its last worktree releases it. `apply --recreate` replaces a shared fork for every worktree that uses it. Older versions of worktreepg treat a shared fork as a database they did not create.

## Commands

```text
git worktreepg apply [--worktree-name <name>] [--recreate] [--terminate] [--dry-run] [--force]
git worktreepg remove [<path>] [--keep-worktree] [--dry-run] [--force]
git worktreepg prune [--dry-run]
git worktreepg list [--all]
git worktreepg template create|refresh|drop [--terminate] [--dry-run] [--force]
```

All commands also accept `--from`, `--include`, `--json`, `--quiet`, and `--verbose`. Run `git worktreepg -h` or `git worktreepg <command> --help` for details.

Useful behavior:

- `apply --recreate` replaces an existing fork; `--terminate` first closes connections the role is allowed to signal.
- `apply --force` overwrites a variable that points somewhere other than its source or fork. Without it, the run stops before creating anything.
- `remove` lets Git reject a dirty or locked worktree before dropping any database. `--keep-worktree` drops only the databases.
- Fork names use the full branch name, lowercase it, and replace non-alphanumeric runs with `_`. Use `--worktree-name` to resolve a collision.
- Managed databases carry a `worktreepg` JSON comment, so `remove` and `prune` ignore databases the tool did not create for this repository. Runs that change one fork take turns on a Postgres advisory lock.
- Exit codes are `0` success, `1` internal error, `2` usage error, `3` conflict, and `4` environment or prerequisite error.

On Postgres 18, `--verbose` reports whether the server and filesystem can use copy-on-write clones. Native APFS on macOS can; Docker Desktop's ext4 volume cannot.

## Development

```sh
cargo test
eval "$(scripts/test-db.sh)" # starts a throwaway Postgres 18
cargo test                  # includes end-to-end tests
scripts/test-db.sh stop
```

Releases use [cargo-dist](https://github.com/axodotdev/cargo-dist): bump `Cargo.toml` and push a `v*` tag.

## License

MIT
