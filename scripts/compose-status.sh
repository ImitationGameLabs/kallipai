#!/usr/bin/env bash
# compose-status.sh -- report whether a running stack's application services
# answer their health routes.
#
#   scripts/compose-status.sh [dev|prod]
#
# The application images carry no compose healthcheck: each holds its binary and
# the CA store, so there is no probe tool inside one for a healthcheck to run.
# The edge image does hold busybox, which is why it is the one that asks on the
# platform network:
#
#   - dev publishes each service on loopback, so the host asks there;
#   - prod keeps the services on the platform network, so the edge container
#     asks, and while the edge is deliberately not started yet a one-shot
#     container from the edge's own image and network asks instead. That image
#     is the reference compose itself resolves, not one written here.
#
# No port is written here either: each service's port is the one in its own
# *_ADDR environment.
#
# Each service is probed on its own health path and judged against what that
# path answers. Every service answers 2xx except instances, whose routes all sit
# behind the platform token in both modes (the base compose file points it at
# the archeion's internal face, and dev does not take that away): there a 401 is
# the proof that the service is up and the guard is armed, since a probe without
# the credential cannot reach more than that. instances also refuses a Host it
# was not configured for, and from inside the platform network it is reached by
# its service name, so both modes put that name in the allowlist the service
# reads (compose.dev.yaml for dev, the deployment's env file for prod).
#
# Exit status: 0 when every service answered as its row expects, 1 when one did
# not (each failure is printed, as is the status that came back), 2 on a usage
# error or a compose file this script cannot read a project or image from.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
env_script="$repo_root/scripts/compose-env.sh"

usage() {
  echo "usage: $0 [dev|prod]" >&2
  exit 2
}

mode=${1:-dev}
case "$mode" in
  dev | prod) ;;
  *) usage ;;
esac

# The project name decides which containers are this stack's; it is written in
# the override as it is used, so nothing has to be interpolated here.
project=$(sed -n 's/^name: *\([^ ]*\)$/\1/p' "compose/compose.$mode.yaml" | head -n 1)
if [ -z "$project" ]; then
  echo "compose-status: compose/compose.$mode.yaml names no project" >&2
  exit 2
fi

# Service -> the environment key carrying the address it listens on, the health
# path, and what that path answers. The gateway listens on two addresses; the
# probe names the management one, where /health sits beside /metrics as the
# process probe. The forwarding face answers its own copy of /health; the prod
# override and the bundle only reach it through the edge, and the dev override
# also publishes 7501 on the loopback for tagmata on this host. A probe through
# the edge would report one caddy failure as every service down, and the process
# exits when either listener cannot bind, so the management address speaks for
# both faces.
services=(
  "archeion:KALLIPAI_ARCHEION_ADDR:/healthz:ok"
  "lesche:KALLIPAI_LESCHE_ADDR:/healthz:ok"
  "keryx:KALLIPAI_KERYX_ADDR:/healthz:ok"
  "files:KALLIPAI_FILES_ADDR:/health:ok"
  "instances:KALLIPAI_INSTANCES_ADDR:/health:401"
  "gateway:KALLIPAI_MODEL_GATEWAY_MANAGEMENT_ADDR:/health:ok"
)

container_of() {
  docker ps -q \
    --filter "label=com.docker.compose.project=$project" \
    --filter "label=com.docker.compose.service=$1" |
    head -n 1
}

address_of() {
  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$1" |
    sed -n "s/^$2=\(.*\)$/\1/p" |
    head -n 1
}

# --- the files and the environment they render with ---------------------------
# Same flags as scripts/compose-up.sh: compose reads the stack's env file
# itself, and the dev fallbacks travel in a file of their own that it overrides.
# Nothing here parses that file; an entry that names the platform network
# reaches both scripts the same way.
env_files=()
defaults_file=""
placeholders_file=""
cleanup() { rm -f "$defaults_file" "$placeholders_file"; }
trap cleanup EXIT
if [ "$mode" = dev ]; then
  defaults_file=$(mktemp)
  bash "$env_script" --defaults dev >"$defaults_file" || exit 2
  env_files+=(--env-file "$defaults_file")
fi
# The values only a start or a deployment knows. They ride in a file of their
# own ahead of that env file, so the deployment's values win while a bare
# checkout still renders; the paths name nothing this script reads or creates.
placeholders_file=$(mktemp)
cat >"$placeholders_file" <<'PLACEHOLDERS'
KALLIPAI_STACK_DIR=/tmp/kallipai-status-stack
TELEMETRY_BASE=/tmp/kallipai-status-telemetry
KALLIPAI_ENV_FILE=/dev/null
KALLIPAI_TLS_DIR=/tmp/kallipai-status-certs
KALLIPAI_INSTANCES_STATE_DIR=/tmp/kallipai-status-instances
GF_SECURITY_ADMIN_PASSWORD=status-probe
KALLIPAI_IMAGE_TAG=unloaded
KALLIPAI_DEV_IMAGE=localhost/kallipai-dev:unloaded
PLACEHOLDERS
env_files+=(--env-file "$placeholders_file")
if [ -f compose/.env ]; then
  env_files+=(--env-file compose/.env)
fi

compose() {
  docker compose "${env_files[@]}" -f compose/compose.yaml -f "compose/compose.$mode.yaml" "$@"
}

# The images this mode resolves, from compose itself: the reference a start
# would use, prefix and tag included, rather than this file repeating the
# syntax of the interpolation.
resolved_images() {
  compose config --images
}

# The platform network's name, out of the same rendering a start would use, so
# an override in the stack's env file reaches the probe. It is the one-shot
# probe's network only; a running edge asks from inside the stack.
if ! rendered=$(compose config --format json); then
  echo "compose-status: compose could not render the $mode stack; the message above names the value it wanted" >&2
  exit 2
fi
network=$(printf '%s' "$rendered" | bash "$env_script" --network) || exit 2
if [ -z "$network" ]; then
  echo "compose-status: compose rendered no external platform network" >&2
  exit 2
fi

edge=$(container_of caddy)

# The HTTP status a service answers on its health path, or nothing when nothing
# answers at all. Dev asks from the host on the published port; prod asks from
# the edge container, or from a one-shot container off the edge's image.
status_of() {
  local service=$1 port=$2 path=$3 url out
  if [ "$mode" = dev ]; then
    url="http://127.0.0.1:$port$path"
    out=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "$url" 2>/dev/null) || return 1
    printf '%s\n' "$out"
    return 0
  fi
  url="http://$service:$port$path"
  if [ -n "$edge" ]; then
    out=$(docker exec "$edge" wget -q -T 5 -S -O /dev/null "$url" 2>&1) || true
  else
    caddy_image=$(resolved_images | grep -m 1 'kallipai-caddy-web') || true
    if [ -z "$caddy_image" ]; then
      echo "compose-status: compose does not resolve an edge image for $mode" >&2
      return 1
    fi
    # The image has to be in the local store already: pulling one as a side
    # effect of a status check hides the real finding behind a network fetch.
    if ! docker image inspect "$caddy_image" >/dev/null 2>&1; then
      echo "compose-status: $caddy_image is not in the local image store; build and load it, or start the edge and ask again" >&2
      return 1
    fi
    out=$(docker run --rm --pull=never --network "$network" "$caddy_image" \
      wget -q -T 5 -S -O /dev/null "$url" 2>&1) || true
  fi
  # busybox wget reports the status and then fails the run, so the code comes
  # out of the message rather than out of the exit status.
  printf '%s\n' "$out" | sed -n 's/.*HTTP\/[0-9.]* \([0-9][0-9][0-9]\).*/\1/p' | tail -n 1
}

failures=0
for entry in "${services[@]}"; do
  IFS=: read -r service key path expect <<<"$entry"

  id=$(container_of "$service")
  if [ -z "$id" ]; then
    echo "compose-status: $service is not running" >&2
    failures=$((failures + 1))
    continue
  fi
  address=$(address_of "$id" "$key")
  port=${address##*:}
  case "$port" in
    '' | *[!0-9]*)
      echo "compose-status: $service has no port in $key ('$address')" >&2
      failures=$((failures + 1))
      continue
      ;;
  esac

  # A probe that cannot answer returns non-zero, and under `set -e` an
  # assignment would end the script right there: the remaining services would
  # go unprobed and no count would be printed.
  status=$(status_of "$service" "$port" "$path") || status=""
  if [ "$expect" = ok ]; then
    case "$status" in
      2??) ruled=1 ;;
      *) ruled=0 ;;
    esac
  else
    if [ "$status" = "$expect" ]; then
      ruled=1
    else
      ruled=0
    fi
  fi

  if [ "$ruled" = 1 ]; then
    if [ "$expect" = ok ]; then
      echo "$service ok (:$port$path)"
    else
      echo "$service up (:$port$path answers $status, the guard is armed)"
    fi
  else
    echo "compose-status: $service answered '$status' on $path where $expect was expected" >&2
    failures=$((failures + 1))
  fi
done

if [ "$failures" -gt 0 ]; then
  echo "compose-status: $failures of ${#services[@]} services did not answer as expected" >&2
  exit 1
fi
echo "compose-status: ${#services[@]} services of $mode answer"
