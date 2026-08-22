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
      "$engine" run -d --rm --name "$name" -p "127.0.0.1:$port:5432" -e POSTGRES_PASSWORD=pw "$image" >/dev/null
    fi
    for _ in $(seq 1 60); do
      if "$engine" exec "$name" pg_isready -U postgres -q 2>/dev/null; then
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
