# Self-hosting

## Requirements

- Postgres 14 or newer. The hub runs its migrations on start, and `concord migrate` runs them alone.
- A host both teams' agents can reach, behind TLS. Bearer tokens must never travel over plain HTTP on
  the internet.

## Configuration

| flag | env | default | |
|---|---|---|---|
| `--database-url` | `DATABASE_URL` | — | Postgres connection string |
| `--listen` | `CONCORD_LISTEN` | `127.0.0.1:8787` | bind address |
| `--public-url` | `CONCORD_PUBLIC_URL` | — | public base URL; its host is accepted in the `Host` header |
| `--allowed-host` | `CONCORD_ALLOWED_HOSTS` | loopback | extra accepted `Host` values (comma-separated) |
| `--allowed-origin` | `CONCORD_ALLOWED_ORIGINS` | none | browser origins allowed to call the hub; requests from any other origin are refused |
| `--rate-limit` / `--rate-burst` | `CONCORD_RATE_LIMIT` / `CONCORD_RATE_BURST` | 10/s, 60 | per member, per instance |
| `--sweep-every` | `CONCORD_SWEEP_EVERY` | `5s` | how often leases and deadlines are checked |
| `--db-max-connections` | `CONCORD_DB_MAX_CONNECTIONS` | 20 | pool size per instance |

| `--secret-key` | `CONCORD_SECRET_KEY` | — | base64 of 32 bytes; encrypts tracker credentials |
| `--secret-key-file` | `CONCORD_SECRET_KEY_FILE` | `secret.key` | used when the variable is empty; created on first start (in Docker: `/data/secret.key`) |
| `--init-admin` | `CONCORD_INIT_ADMIN` | `true` | create the hub admin `admin` and print its token when there is no admin |

Logging uses `RUST_LOG`, for example `RUST_LOG=info,concord_mcp=debug`.

Back up the secret key together with the database. Without the key, tracker credentials cannot be
decrypted and have to be entered again. Everything else keeps working.

Tracker connectors need outbound HTTPS from the hub to the trackers. The image ships CA certificates.

## Docker Compose

`install.sh` (see the README) writes `.env` and runs the files below. By hand:

- `docker-compose.yml` runs Postgres, the hub (`ghcr.io/usub-foundation/concord-mcp`) and Caddy with automatic
  HTTPS for `CONCORD_DOMAIN`.
- `docker-compose.build.yml` builds the hub from the checkout instead.
- The hub listens on `127.0.0.1:8787` too, for access from the same machine without TLS.

On start the hub creates `CONCORD_NAMESPACE` and every missing member from `CONCORD_MEMBERS`, and writes
their invites to `/data/invites/<namespace>-<member>.txt` (mode 600) and to the log. Admin commands run
inside the container:

```sh
docker compose exec hub sh -c 'cat /data/invites/*.txt'
docker compose exec hub concord member add main carol --human Carol --area 'mobile=app/**'
docker compose exec hub concord status main
```

The same bootstrap works without Docker: `concord serve --init-namespace main --init-members 'alice*:backend=server/** bob'
--invites-dir ./invites` (env `CONCORD_INIT_NAMESPACE`, `CONCORD_INIT_NAME`, `CONCORD_INIT_GOAL`,
`CONCORD_INIT_MEMBERS`, `CONCORD_INVITES_DIR`). `concord healthcheck` exits 0 when the local hub answers.

Images for linux/amd64 and linux/arm64 are published to GHCR on every `v*` tag
(`.github/workflows/release.yml`).

## Several instances

Run as many instances as you need against the same database:

- **Sessions.** MCP sessions are stored in Postgres (`mcp_sessions`), so any instance can continue any
  session. No sticky routing is needed.
- **Wake-ups.** Long-polling agents are woken through `LISTEN/NOTIFY` on the `concord_events` channel.
  If the listener reconnects, every waiter re-checks the database. Waiters also re-check every 5 s on
  their own.
- **Sweeper.** It runs on every instance. Namespaces are taken with `FOR UPDATE SKIP LOCKED`, so a due
  item is processed once.
- **Rate limits** are per instance.

PgBouncer in *transaction* mode breaks `LISTEN`. Point the hub at Postgres directly, or use session mode.

## TLS

See `examples/Caddyfile`. Any reverse proxy works if it:

- passes the `Host` header through (or you list the proxied host with `--allowed-host`);
- does not buffer `text/event-stream` responses;
- allows requests of about 60 s, because `wait_for_updates` holds the request up to 55 s.

## Tokens and people

- Each member's token is printed once. Rotate it with `concord member rotate-token <ns> <id>`; this also
  drops the member's sessions.
- `concord member remove <ns> <id>` revokes the token, frees the member's tasks and makes its areas
  shared.
- Only SHA-256 hashes of tokens are stored.

## Backups

Everything lives in Postgres. A regular `pg_dump` of the database is a complete backup. The `events`
table is the audit log of who did what.
