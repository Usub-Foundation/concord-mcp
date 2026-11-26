#!/usr/bin/env sh
# Disposable Postgres for `cargo test` on 127.0.0.1:55440 (removed when stopped).
#   scripts/test-db.sh          start
#   scripts/test-db.sh stop     stop and remove
set -eu
NAME=concord-test-pg
if [ "${1:-}" = "stop" ]; then
  docker stop "$NAME" >/dev/null && echo "stopped $NAME"
  exit 0
fi
if docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "$NAME is already running"
else
  docker run -d --rm --name "$NAME" --memory=1g \
    -e POSTGRES_USER=concord -e POSTGRES_PASSWORD=concord -e POSTGRES_DB=concord \
    -p 127.0.0.1:55440:5432 postgres:17-alpine \
    -c max_connections=300 -c fsync=off -c synchronous_commit=off -c full_page_writes=off >/dev/null
  until docker exec "$NAME" pg_isready -U concord >/dev/null 2>&1; do sleep 0.5; done
  echo "started $NAME"
fi
echo "export DATABASE_URL=postgres://concord:concord@127.0.0.1:55440/concord"
