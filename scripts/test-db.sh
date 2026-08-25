#!/usr/bin/env sh
# Starts a throwaway Postgres 18 in podman (or docker) for the end-to-end test and prints the
# URL to export as WORKTREE_PG_TEST_URL. Stop it with: scripts/test-db.sh stop
set -eu

name=worktreepg-test
port=${WORKTREE_PG_TEST_PORT:-54320}
image=docker.io/library/postgres:18

if command -v podman >/dev/null 2>&1; then
  engine=podman
elif command -v docker >/dev/null 2>&1; then
  engine=docker
else
  echo "neither podman nor docker is installed" >&2
  exit 1
fi

case "${1:-start}" in
  start)
    if ! "$engine" container exists "$name" 2>/dev/null && ! "$engine" inspect "$name" >/dev/null 2>&1; then
      # autovacuum is off because worktreepg counts connections through pg_stat_activity, which
      # lists autovacuum workers alongside client backends, and the DDL the tests run attracts
      # them. The counts the tests assert on are exact. The setting belongs to this throwaway
      # cluster: the tests never change the configuration of the cluster they are pointed at.
      "$engine" run -d --rm --name "$name" -p "127.0.0.1:$port:5432" -e POSTGRES_PASSWORD=pw "$image" -c autovacuum=off >/dev/null
    fi
    for _ in $(seq 1 60); do
      if "$engine" exec "$name" pg_isready -U postgres -q 2>/dev/null; then
        # A container started before this flag existed keeps the settings it was started with,
        # and nothing else here would say why the counts flake.
        if [ "$("$engine" exec "$name" psql -U postgres -tAc 'SHOW autovacuum' 2>/dev/null)" != "off" ]; then
          echo "warning: $name is running with autovacuum on; run \"$0 stop\" first, or expect flaky connection counts" >&2
        fi
        echo "export WORKTREE_PG_TEST_URL=postgres://postgres:pw@127.0.0.1:$port/postgres"
        exit 0
      fi
      sleep 0.5
    done
    echo "postgres did not become ready" >&2
    exit 1
    ;;
  stop)
    "$engine" stop "$name" >/dev/null 2>&1 || true
    ;;
  *)
    echo "usage: $0 [start|stop]" >&2
    exit 2
    ;;
esac
