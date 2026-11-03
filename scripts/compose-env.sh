#!/usr/bin/env bash
# The values the compose files do not carry themselves, in one place: the values
# a dev stack derives for itself, the starting compose/.env the docs point at,
# image list both scripts load, and the platform network's name.
#
# compose.yaml, compose.dev.yaml and compose.prod.yaml spell `${NAME:?...}` for
# every value that has no safe default. Those files are the list of what is
# required: scripts/compose-up.sh hands them to compose and lets compose report
# a missing name, so no second copy of the list lives here.
#
# The stack's env file, compose/.env, is read by compose, not by this script.
# Quotes, comments, inline comments and expansion inside it follow compose's
# own rules, and scripts/compose-up.sh gets the resolved values back from
# `docker compose config`. A bash reader would have to agree with compose on
# every one of those cases, and one disagreement hands one side a value the
# other never sees (an origin pointing at a stale domain, a network nobody
# created).
#
# The app origin is not derived here either: compose.dev.yaml derives it from
# KALLIPAI_DOMAIN the way compose.prod.yaml always has, so a domain set in
# compose/.env still yields the origin the browser types. Compose derives
# `app.<domain>` on 443, so any edge that answers under another host, off 443,
# or over http has to name it.
#
# Usage:
#   scripts/compose-env.sh --defaults dev     the values a dev stack can
#                                             compute for itself (prod has
#                                             none: every prod value is a
#                                             deployment decision)
#   scripts/compose-env.sh --example dev|prod NAME=value lines for compose/.env
#   scripts/compose-env.sh --images all|dev|prod
#                                             the image short names that mode
#                                             runs, in deploy order
#   scripts/compose-env.sh --network          the platform network's name, read
#                                             from a rendered config on stdin
#                                             (docker compose config --format
#                                             json); needs python3
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# The dev values, derived in one place: --defaults dev is the fallback env file
# scripts/compose-up.sh hands compose (the stack's env file and the caller's
# environment both win over it), and --example dev prints the same values as
# a starting compose/.env. The daemon socket directory follows the dev override
# (compose.dev.yaml): $XDG_RUNTIME_DIR/kallipai/daemon, the runtime-leg socket a
# daemon binds. The domain falls back to kallipai.lan, the name the mkcert pair
# in compose/dev/.certs carries: a fallback the certificate does not cover would
# fail the edge's own hostname check before any request is made.
#
# The edge serves https, and 443 is the one port a browser leaves out of the
# URL it types, so compose.dev.yaml derives the app origin without a port.
# Compose derives `app.<domain>` on 443, so an edge that answers under another
# host, off 443, or over http has to name KALLIPAI_APP_ORIGIN itself, and
# scripts/compose-up.sh refuses it without one.
dev_values() {
  local runtime=$1 domain=${KALLIPAI_DOMAIN:-kallipai.lan}
  local edge_port=${KALLIPAI_EDGE_PORT:-443}
  cat <<VALUES
KALLIPAI_DOMAIN=$domain
KALLIPAI_EDGE_PORT=$edge_port
KALLIPAI_DAEMON_STATE_DIR=$runtime/kallipai/daemon
KALLIPAI_TAGMATA_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/kallipai/tagmata
TELEMETRY_BASE=${XDG_DATA_HOME:-$HOME/.local/share}/kallipai/telemetry
VALUES
}

# Values dev can compute for itself. Prod has none: every prod value is a
# deployment decision, so `--defaults prod` prints nothing.
defaults() {
  case "${1:-}" in
    dev)
      if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
        echo "compose-env: XDG_RUNTIME_DIR is unset; the instances socket bind needs it (or set KALLIPAI_DAEMON_STATE_DIR yourself)" >&2
        return 1
      fi
      dev_values "$XDG_RUNTIME_DIR"
      ;;
    prod) ;;
    *)
      echo "usage: $0 --defaults dev" >&2
      return 2
      ;;
  esac
}

# Values an operator can start from. They are examples,
# not defaults: compose-up.sh computes the real ones from the checkout and the
# nix build outputs, and prod values must come from the deployment.
# KALLIPAI_IMAGE_TAG is not printed: only the tag scripts/compose-up.sh loaded
# identifies images that exist in the store.
# KALLIPAI_DEV_IMAGE is a placeholder: scripts/compose-up.sh replaces it with
# the reference it just loaded.
# KALLIPAI_APP_ORIGIN is not printed here: compose.dev.yaml derives it from the
# domain, and a frozen copy in the stack's env file is exactly the value that
# goes stale when the domain changes.
example() {
  case "${1:-}" in
    dev)
      cat <<EXAMPLES
KALLIPAI_STACK_DIR=$repo_root/.stack
KALLIPAI_DEV_IMAGE=localhost/kallipai-dev:dev
EXAMPLES
      # The runtime dir is a placeholder here: an example is printed where no
      # session may exist, and the real value comes from --defaults.
      dev_values "${XDG_RUNTIME_DIR:-/run/user/1000}"
      ;;
    prod)
      cat <<EXAMPLES
KALLIPAI_STACK_DIR=/var/lib/kallipai/stack
TELEMETRY_BASE=/var/lib/kallipai/telemetry
GF_SECURITY_ADMIN_PASSWORD=change-me
KALLIPAI_DOMAIN=example.com
# The gateway's published base follows KALLIPAI_DOMAIN; uncomment to point
# the forwarding face at another host:
# KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL=https://model-gw.example.com/v1
KALLIPAI_ENV_FILE=$repo_root/compose/.env.prod
KALLIPAI_INSTANCES_STATE_DIR=/run/user/<uid>/kallipai/daemon
KALLIPAI_TLS_DIR=/etc/kallipai/tls
EXAMPLES
      ;;
    *)
      echo "usage: $0 --example dev|prod" >&2
      return 2
      ;;
  esac
}

# The images a mode runs, by short name in deploy order; the flake attribute
# is kallipai-<name>-image. `all` is what scripts/build-and-load-images.sh
# builds, `dev` and `prod` are what scripts/compose-up.sh loads. One list, so a
# service added to the stack cannot be missing from one script and loaded by
# the other. The observability services are in no list: their images come from
# upstream registries, pinned by digest in the compose overrides.
images() {
  case "${1:-}" in
    all)
      printf '%s\n' archeion lesche keryx files model-gateway instances \
        caddy-web tagma
      ;;
    prod) printf '%s\n' archeion lesche files keryx model-gateway instances caddy-web ;;
    dev) printf '%s\n' caddy-web ;;
    *)
      echo "compose-env: --images takes all, dev or prod" >&2
      return 2
      ;;
  esac
}

# The platform network's name, out of the config compose rendered: compose.yaml
# declares the network external with a default and the stack's env file may
# name another, so the name is whatever compose says, not a second reading.
# scripts/compose-up.sh creates the network under this name and
# scripts/compose-status.sh probes inside it.
network() {
  local rendered
  if ! command -v python3 >/dev/null 2>&1; then
    echo "compose-env: python3 is required to read the platform network's name" >&2
    return 1
  fi
  rendered=$(cat)
  if [ -z "$rendered" ]; then
    echo "compose-env: --network reads the rendered config on stdin and got nothing" >&2
    return 2
  fi
  printf '%s' "$rendered" | python3 -c '
import json
import sys

try:
    document = json.load(sys.stdin)
except ValueError as error:
    sys.stderr.write("compose-env: the config on stdin is not the JSON compose renders: %s\n" % error)
    raise SystemExit(2)
names = [
    network.get("name")
    for network in (document.get("networks") or {}).values()
    if network.get("external") and network.get("name")
]
print(names[0] if names else "")
'
}

case "${1:-}" in
  --defaults) defaults "${2:-}" ;;
  --example) example "${2:-}" ;;
  --images) images "${2:-}" ;;
  --network) network ;;
  *)
    echo "usage: $0 --defaults dev | --example dev|prod | --images all|dev|prod | --network" >&2
    exit 2
    ;;
esac
