#!/bin/sh
# concord-mcp one-line installer: Postgres + hub + automatic HTTPS with Docker Compose.
#
#   curl -fsSL https://raw.githubusercontent.com/Usub-Foundation/concord-mcp/main/install.sh | sh
#
# It asks for the domain and the members, writes ./concord/.env, starts everything and prints
# one invite (token + `claude mcp add ...`) per member. Re-running it keeps the existing .env.
# Non-interactive: set CONCORD_DOMAIN, CONCORD_MEMBERS (and optionally CONCORD_NAMESPACE,
# CONCORD_NAMESPACE_NAME, CONCORD_GOAL, CONCORD_BIND_ADDR, CONCORD_HTTP_PORT, CONCORD_HTTPS_PORT,
# CONCORD_LOCAL_PORT, CONCORD_IMAGE)
# and CONCORD_NONINTERACTIVE=1. CONCORD_BUILD=1 builds the image from a source checkout.
set -eu

REPO="${CONCORD_REPO:-Usub-Foundation/concord-mcp}"
REF="${CONCORD_REF:-main}"
DIR="${CONCORD_DIR:-$PWD/concord}"
RAW="https://raw.githubusercontent.com/$REPO/$REF"

say() { printf '%s\n' "$*"; }
die() { say "error: $*" >&2; exit 1; }

# ask VAR "question" "default": keeps VAR if already set, reads /dev/tty otherwise.
ask() {
  eval "cur=\${$1:-}"
  [ -n "$cur" ] && return 0
  ans=""
  if [ -z "${CONCORD_NONINTERACTIVE:-}" ] && [ -r /dev/tty ]; then
    printf '%s [%s]: ' "$2" "$3" >/dev/tty
    read -r ans </dev/tty || ans=""
  fi
  [ -n "$ans" ] || ans="$3"
  eval "$1=\$ans"
}

command -v docker >/dev/null 2>&1 || die "docker is required: https://docs.docker.com/engine/install/"
if docker compose version >/dev/null 2>&1; then
  DC="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
  DC="docker-compose"
else
  die "the docker compose plugin is required: https://docs.docker.com/compose/install/"
fi

# Use files next to this script when run from a checkout, otherwise download them.
SRC=""
case "$0" in
  */install.sh) SRC=$(cd "$(dirname "$0")" && pwd) ;;
esac
fetch() {
  if [ -n "$SRC" ] && [ -f "$SRC/$1" ]; then
    cp "$SRC/$1" "$2"
  else
    curl -fsSL "$RAW/$1" -o "$2" || die "cannot download $RAW/$1"
  fi
}

if [ -n "${CONCORD_BUILD:-}" ]; then
  [ -n "$SRC" ] || die "CONCORD_BUILD=1 needs a source checkout: git clone the repo and run ./install.sh there"
  DIR="${CONCORD_DIR:-$SRC}"
fi
if [ -n "$SRC" ] && [ "$SRC" = "$(cd "$DIR" 2>/dev/null && pwd || true)" ]; then
  : # installing into the checkout itself
else
  mkdir -p "$DIR/deploy"
  fetch docker-compose.yml "$DIR/docker-compose.yml"
  fetch docker-compose.build.yml "$DIR/docker-compose.build.yml"
  fetch deploy/Caddyfile "$DIR/deploy/Caddyfile"
fi
cd "$DIR"

if [ ! -f .env ]; then
  ask CONCORD_DOMAIN "Domain of the hub (its DNS must point here; localhost for a local test)" "localhost"
  ask CONCORD_NAMESPACE "Namespace id" "main"
  ask CONCORD_NAMESPACE_NAME "Project name" "$CONCORD_NAMESPACE"
  ask CONCORD_MEMBERS "Members: id[*][:area=globs], space separated, * = lead" "alice*:backend=server/** bob:frontend=web/**"
  CONCORD_GOAL="${CONCORD_GOAL:-}"
  port="${CONCORD_HTTPS_PORT:-443}"
  public="https://$CONCORD_DOMAIN"
  [ "$port" = "443" ] || public="$public:$port"
  password=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
  umask 077
  {
    say "CONCORD_DOMAIN=$CONCORD_DOMAIN"
    say "CONCORD_PUBLIC_URL=$public"
    say "CONCORD_DB_PASSWORD=$password"
    say "CONCORD_NAMESPACE=$CONCORD_NAMESPACE"
    say "CONCORD_NAMESPACE_NAME=$CONCORD_NAMESPACE_NAME"
    say "CONCORD_GOAL=$CONCORD_GOAL"
    say "CONCORD_MEMBERS=$CONCORD_MEMBERS"
    for v in CONCORD_IMAGE CONCORD_BIND_ADDR CONCORD_HTTP_PORT CONCORD_HTTPS_PORT CONCORD_LOCAL_PORT; do
      eval "val=\${$v:-}"
      [ -z "$val" ] || say "$v=$val"
    done
  } >.env
  say "wrote $DIR/.env"
fi

if [ -n "${CONCORD_BUILD:-}" ]; then
  $DC -f docker-compose.yml -f docker-compose.build.yml up -d --build
else
  $DC up -d
fi

say "waiting for the hub..."
i=0
until $DC exec -T hub concord healthcheck >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -lt 90 ] || die "the hub did not become healthy; see: $DC logs hub"
  sleep 2
done

invites=$($DC exec -T hub sh -c 'cat /data/invites/*.txt 2>/dev/null' || true)
say ""
url=$(grep '^CONCORD_PUBLIC_URL=' .env | cut -d= -f2-)
say "concord is running"
say "  web UI:  $url/   (sign in with the hub admin token below)"
say "  agents:  $url/mcp/<namespace>"
if [ -n "$invites" ]; then
  say "The hub admin token is for you. Send each other person ONLY their own invite (tokens are secrets):"
  say ""
  say "$invites"
fi
say ""
say "More members later:  cd $DIR && $DC exec hub concord member add <namespace> <id> --area 'name=glob'"
say "Plan as markdown:    cd $DIR && $DC exec hub concord status <namespace>"
