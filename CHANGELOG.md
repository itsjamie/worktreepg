# Changelog

## Unreleased

Repositories that point at one development database can share one fork, which is what a workerbox worker spanning several repositories wants.

A directive that names a group, as in `# worktreepg: .env DATABASE_URL share=shop`, lets the fork it makes be joined by a worktree of another repository whose directive names the same group, and lets it join one. The token can go anywhere in the directive and works with every existing form. Both sides have to opt in: a fork created without a group is refused exactly as before, a directive without a group cannot join one, and a group admits only directives that name it. The refusal now names the other repository's worktree and says which directive needs `share=`. A shared fork records every worktree that uses it, at most one per repository, so two worktrees of one repository whose names collide are still refused. A fork's only owner can add, change, or remove its group by re-running `apply`, which is how a fork created before its repository named a group becomes joinable.

`remove` and `prune` release this repository's worktree from a shared fork and drop the fork only when no worktree is left. `prune` considers only this repository's worktrees; another repository's dead worktree is released by that repository's own `prune`. `apply --recreate` on a shared fork replaces it for every worktree that uses it, keeps them all as owners, and says on stderr whose data it replaced.

Every run that creates, joins, recreates, releases, or drops a fork holds a Postgres advisory lock on the fork's name while it decides and acts. Two applies that want one fork therefore take turns: the second waits for the first's clone and then joins it, rather than failing on `CREATE DATABASE` or reading a fork with no comment yet as foreign. A join and the last release are serialized the same way, so the release cannot drop a fork that a join has just recorded itself on.

### Worth knowing before upgrading

- A fork with one owner and no group is still written in the format 0.4 reads. A shared fork uses format 2 of the comment, which 0.4 reads as a database it did not create, so it neither adopts nor drops the fork.
- `apply` reports `joined` (`join_planned` in a dry run) and a `join_database` action. `remove` and `prune` report `released` and a `release_database` action. `list` rows gain `share` and `owners`; their `repo`, `worktree`, and `worktree_exists` describe this repository's worktree.

## 0.4.0

This release tightens the boundaries around database ownership, filesystem paths, env-file updates, and PostgreSQL endpoints.

- Templates are scoped to both repository and source database, and forks that collide across sources are refused instead of adopted.
- Derived names keep their identifying suffix at PostgreSQL's identifier limit.
- Directive, include, source, and target paths cannot escape their worktree through traversal or symlinks.
- Env-file rewrites preserve concurrent edits and encode database names correctly in URLs.
- Ambiguous multi-host and multi-port endpoints are rejected, and invalid configuration consistently exits as an environment error.
- `template create --force` now performs the documented takeover, while failed template drops restore their original flags.
- `remove` works from a subdirectory and leaves the worktree intact when its source URL is invalid.
- The README now contains the working essentials without the implementation tour.

## 0.3.0

Mixed-credential directives work, and a command that cannot finish its work now says which piece it could not do instead of stopping part way through.

### Clusters with more than one role

Each database is forked, snapshotted, or dropped once, as the role in the first directive URL that names it, and connections are pooled per role. A cluster holding two databases owned by two roles gets one connection each and still does each piece of work once, which is what made `apply --recreate` and `template refresh` usable on a repository whose directives mix a superuser with a runtime role.

A statement Postgres refuses for want of privileges names the role it ran as, which is not always the role in the variable being applied, and names the other roles the directives offer for that database.

### Nothing is created before everything that can be decided has been

`apply` opens the target env files first and settles every variable against them, so a variable pointing at some third database stops the run with nothing created and nothing written. It used to find that out after every fork existed, and exit leaving a database `prune` would not reclaim and no command could name.

Past that, a database that fails for a reason no connection could predict is reported, counted, and the run carries on, so the databases that did fork get their variables rewritten rather than being left pointing at the live database. `remove` connects to every database its directives name before `git worktree remove` runs, so a server that is down stops it before the worktree is gone. `prune` and `remove` skip a fork none of the offered roles owns rather than failing every later run on it, and say what brings it back within reach.

### Reporting

The create line says where a fork came from, `origin=live` or `origin=template` with the snapshot's age, so a wrapper no longer has to sniff the version to know which behaviour it got.

A connection count is reported only where the role can see one. `pg_stat_activity` hides what a session is from a role holding neither that session's role's privileges nor `pg_read_all_stats`, and a count that includes a row it cannot identify is an upper bound rather than a number.

`--terminate` closes the connections the role can close instead of failing on the first backend it may not signal. A backend that survives is left to `CREATE DATABASE`, which refuses over what is attached when it runs.

### Worth knowing before upgrading

- Two branches whose names normalize to one fork name (`feature/auth-v2` and `feature/auth.v2`, or any two agreeing to the identifier limit) are a conflict naming the worktree that owns the fork. They used to share one database quietly, and `apply --recreate` in the second dropped the first's data.
- The `server` field in `--json` `list` rows is the cluster (`host:port`) and no longer carries a role.
- A refused `--terminate` no longer fails the run: `apply` falls back to the snapshot the way an in-use source has always meant, so a run that exited non-zero can now exit 0 with `origin=template` and older data. The warning that says so goes to stderr and survives `--quiet`.

## 0.2.0

Forks are cloned from the live database, with the snapshot as a fallback when something is connected to it.

## 0.1.0

First release.
