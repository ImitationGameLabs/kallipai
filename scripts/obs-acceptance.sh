#!/usr/bin/env bash
# Acceptance for the telemetry intake: one door, one platform token, and the
# two stores behind it. Each check reads a store, so a config that parses but
# drops the wrong thing fails here instead of in a dashboard.
#
#   scripts/obs-acceptance.sh [--keep] [--only 3,5] [--root DIR]
#
# One prepared step runs before the numbered checks: the config gate. The
# rendered collector text is validated by the image that runs it, and a
# missing image ends the run with the reason instead of waiting for a pull.
# That gate is a prerequisite: `--only` skips none of it.
#
# The run brings up tempo, prometheus and the collector under its own compose
# project from this checkout's rendered texts, starts two `kallipai-tagma`
# processes from its build, and lets them push to the one door. Ports 14318
# (door), 14133 (collector health), 13200 (tempo), 19090 (prometheus) and
# 7301/7302 (the instances' own addresses) must be free: stop the dev stack
# first.
#
# Everything lives under --root (a fresh temporary directory by default) and is
# removed on success unless --keep. A failed check leaves the root in place and
# prints where it is.
#
# The Prometheus port is published here and nowhere in the deployed stack: this
# script is the reader, and a reader on loopback is the point.

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
project=kallipai-obs-acceptance
slugs=(acc-a acc-b)
door_port=14318
health_port=14133
tagma_ports=(7301 7302)
prom_port=19090
tempo_port=13200
keep=0
only=""
root=""

usage() {
  sed -n '2,26p' "${BASH_SOURCE[0]}" >&2
  exit 2
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --keep) keep=1 ;;
    --only) only=$2; shift ;;
    --root) root=$2; shift ;;
    -h | --help) usage ;;
    *) echo "obs-acceptance: unknown argument $1" >&2; usage ;;
  esac
  shift
done

say() { printf 'obs-acceptance: %s\n' "$*"; }
fail() {
  say "$*"
  exit 1
}

# The pinned references the compose stack runs, read from the dev override so
# this script cannot drift to a second version of any image.
image_of() {
  python3 - "$repo_root/compose/compose.dev.yaml" "$1" <<'PY'
import sys
path, service = sys.argv[1], sys.argv[2]
image = None
current = None
for line in open(path):
    stripped = line.strip()
    if line.startswith("  ") and not line.startswith("    ") and stripped.endswith(":"):
        current = stripped[:-1]
        continue
    if current == service and stripped.startswith("image: "):
        image = stripped[len("image: "):]
        break
if image is None:
    raise SystemExit("obs-acceptance: %s has no image line" % service)
print(image)
PY
}

instances_before=$(
  docker compose -p "$project" ps --format json 2>/dev/null | python3 -c 'import json,sys; d=sys.stdin.read().strip(); print(len(json.loads(d)) if d else 0)' 2>/dev/null || echo 0
)
if [ "$instances_before" != "0" ]; then
  say "project $project is already up; bring it down with:"
  say "  docker compose -p $project down -v"
  exit 1
fi

if [ -z "$root" ]; then
  root=$(mktemp -d "${TMPDIR:-/tmp}/kallipai-obs-acceptance.XXXXXX")
else
  mkdir -p "$root"
fi
token_file=$root/intake-token
stack_dir=$root/stack

say "root $root"
mkdir -p "$root/instances" "$root/logs" "$root/run" "$root/prom" "$root/tempo"
chmod 777 "$root/prom"

# One platform credential: the collector reads this file and every tagma sends
# it. A run that rotates it would rotate both sides, which is what the file is
# for.
token=$(python3 -c 'import secrets; print("acc-" + secrets.token_hex(16))')
printf '%s' "$token" >"$token_file"
chmod 600 "$token_file"
# The collector runs as root inside its container and reads the file by its
# host path, so the path leading to it has to be traversable.
chmod a+rX "$root"

# Each instance stores its enrollment as one relay entry: credentials/<entry>/
# under the data root, named by the [[polis]] entry in polis.toml. The id and
# the token come from that one entry, so what a process reports and what it
# authenticates with cannot drift. The url is never dialled here; it only has
# to parse.
for i in 0 1; do
  slug=${slugs[$i]}
  data=$root/instances/$slug
  mkdir -p "$data/credentials/acc"
  printf '[[polis]]\nname = "acc"\nurl = "http://127.0.0.1:9"\n' >"$data/polis.toml"
  printf '%s' "$slug" >"$data/credentials/acc/tagma.id"
  printf '%s' "$token" >"$data/credentials/acc/tagma.token"
  chmod 600 "$data/credentials/acc/tagma.id" "$data/credentials/acc/tagma.token"
done

say "rendering the stack texts"
stack_dir=$(cd "$repo_root" && nix build .#kallipai-stack --no-link --print-out-paths)
[ -f "$stack_dir/otelcol.yaml" ] || fail "the stack build wrote no collector text"

# The image that runs the text is the one that validates it. A missing image
# ends the run here with the reason: the registry is not reachable from this
# host, so waiting for a pull would only turn a missing image into a timeout.
collector_image=$(image_of otelcol)
if ! docker image inspect "$collector_image" >/dev/null 2>&1; then
  say "the pinned collector image is not in the local store: $collector_image"
  say "docker cannot fetch it (the registry is unreachable from this host), so the rendered text cannot be validated"
  exit 1
fi
say "validating the rendered collector text with $collector_image"
docker run --rm --user "0:0" \
  -v "$stack_dir/otelcol.yaml:/cfg.yaml:ro" \
  -v "$token_file:/etc/kallipai/intake-token:ro" \
  "$collector_image" validate --config=/cfg.yaml
say "the collector accepts the rendered text"

# --- the stack ----------------------------------------------------------------
cat >"$root/compose.yaml" <<YAML
name: $project
services:
  tempo:
    image: $(image_of tempo)
    command: ["-config.file=/etc/tempo.yaml"]
    user: "0:0"
    volumes:
      - $stack_dir/tempo.yaml:/etc/tempo.yaml:ro
      - $root/tempo:/var/tempo
    ports: ["127.0.0.1:$tempo_port:3200"]
  prometheus:
    image: $(image_of prometheus)
    command:
      - --config.file=/etc/prometheus/prometheus.yml
      - --storage.tsdb.path=/prometheus
      - --web.enable-remote-write-receiver
    volumes:
      - $stack_dir/prometheus.yaml:/etc/prometheus/prometheus.yml:ro
      - $root/prom:/prometheus
    ports: ["127.0.0.1:$prom_port:9090"]
  otelcol:
    image: $collector_image
    user: "0:0"
    depends_on: [tempo]
    volumes:
      - $stack_dir/otelcol.yaml:/etc/otelcol-contrib/config.yaml:ro
      - $token_file:/etc/kallipai/intake-token:ro
    ports:
      - "127.0.0.1:$door_port:4318"
      - "127.0.0.1:$health_port:13133"
YAML

declare -a tagma_pids=()
start_instance() {
  local i=$1
  local slug=${slugs[$i]}
  local data=$root/instances/$slug
  local log=$root/logs/$slug.log
  KALLIPAI_TAGMA_SLUG=$slug \
    KALLIPAI_TAGMA_DATA_DIR=$data \
    KALLIPAI_TAGMA_ADDR=127.0.0.1:${tagma_ports[$i]} \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:$door_port \
    RUST_LOG=info \
    "$tagma_bin" >"$log" 2>&1 &
  tagma_pids[$i]=$!
  echo "$slug ${tagma_pids[$i]}" >>"$root/run/instances"
}

stop_instance() {
  local i=$1
  local pid=${tagma_pids[$i]:-}
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}

teardown() {
  local status=$?
  local i
  for i in 0 1; do stop_instance "$i"; done
  docker compose -p "$project" -f "$root/compose.yaml" down -v >/dev/null 2>&1 || true
  if [ "$status" = 0 ] && [ "$keep" = 0 ]; then
    chmod -R u+rwX "$root" 2>/dev/null || true
    rm -rf "$root" 2>/dev/null || true
  else
    say "kept $root (logs in $root/logs, run state in $root/run)"
  fi
  exit "$status"
}
trap teardown EXIT

probe() { python3 "$repo_root/scripts/obs-acceptance-probe.py" "$@"; }
prom_url=http://127.0.0.1:$prom_port
tempo_url=http://127.0.0.1:$tempo_port

say "building kallipai-tagma"
tagma_bin=$(cd "$repo_root" && nix build .#packages.x86_64-linux.kallipai-tagma --no-link --print-out-paths)/bin/kallipai-tagma
[ -x "$tagma_bin" ] || fail "the build produced no kallipai-tagma"

say "starting tempo, prometheus and the collector"
docker compose -p "$project" -f "$root/compose.yaml" up -d >/dev/null

# The door answers a push only once its receiver is bound, and a published
# port with nothing behind it closes the connection instead. Wait for the
# health face, so the first check measures the door and not its startup.
say "waiting for the collector's health face"
probe wait "http://127.0.0.1:$health_port/" 60 >/dev/null || fail "the collector never answered on its health face"

say "starting ${slugs[*]}"
: >"$root/run/instances"
start_instance 0
start_instance 1

# Every failing check is reported and the run continues, so one --only 3 run
# tells you about check 3 alone and a full run tells you everything at once.
failed=()
selected() {
  [ -z "$only" ] && return 0
  case ",$only," in
    *",$1,"*) return 0 ;;
    *) return 1 ;;
  esac
}
report() {
  local number=$1
  local title=$2
  shift
  shift
  if "$@"; then
    say "check $number ($title): PASS"
  else
    say "check $number ($title): FAIL"
    failed+=("$number")
  fi
}

hit() { probe wait "http://127.0.0.1:$1/agents/root" 10 >/dev/null 2>&1 || true; }
ids_from_prom() {
  probe prom "$prom_url" "$1" |
    python3 -c 'import re,sys; print("\n".join(sorted({m for line in sys.stdin for m in re.findall(r"tagma_id=([^, ]*)", line) if m})))'
}
# One series value: the last field of the only row the expression answers.
value_of() {
  local rows
  rows=$(probe prom "$prom_url" "$1") || return 1
  printf '%s' "$rows" | python3 -c 'import sys; v = sys.stdin.read().split(); print(v[-1] if v else "")'
}
tempo_rows() { probe tempo "$tempo_url"; }

wait_prom() {
  local expr=$1
  local secs=$2
  local deadline
  deadline=$(($(date +%s) + secs))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ -n "$(probe prom "$prom_url" "$expr")" ]; then return 0; fi
    sleep 2
  done
  return 1
}

# The value of one series once it exists at all, or an empty string.
wait_value() {
  local expr=$1
  local secs=$2
  local deadline value
  deadline=$(($(date +%s) + secs))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    value=$(value_of "$expr" || true)
    if [ -n "$value" ]; then printf '%s' "$value"; return 0; fi
    sleep 2
  done
  printf '%s' ""
}

# The same series once it has grown past a baseline.
wait_growth() {
  local expr=$1
  local baseline=$2
  local secs=$3
  local deadline value
  deadline=$(($(date +%s) + secs))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    value=$(value_of "$expr" || true)
    if [ -n "$value" ] && python3 -c 'import sys; raise SystemExit(0 if float(sys.argv[1]) > float(sys.argv[2]) else 1)' "$value" "$baseline"; then
      printf '%s' "$value"
      return 0
    fi
    sleep 2
  done
  printf '%s' ""
}

# The door answers one credential and no other. A token the text does not carry,
# and no token at all, both have to be refused: a door that accepts either is a
# door with no boundary left to check.
check1() {
  local pushed foreign empty
  pushed=$(probe push "$door_port" "$token" trace "${slugs[0]}")
  say "the platform token answered $(printf '%s' "$pushed" | cut -d' ' -f1)"
  [ "$(printf '%s' "$pushed" | cut -d' ' -f1)" = 200 ] || return 1
  foreign=$(probe push "$door_port" "not-${token}" trace "${slugs[1]}")
  say "a foreign token answered $(printf '%s' "$foreign" | cut -d' ' -f1)"
  [ "$(printf '%s' "$foreign" | cut -d' ' -f1)" = 401 ] || return 1
  empty=$(probe push "$door_port" "" trace "${slugs[1]}")
  say "no token answered $(printf '%s' "$empty" | cut -d' ' -f1)"
  [ "$(printf '%s' "$empty" | cut -d' ' -f1)" = 401 ]
}

# One push with a resource attribute of its own reaches both backends: the
# trace store by id, the metrics store by the tagma_id the conversion puts on
# the series. This is what one door with two pipelines has to do.
check2() {
  local pushed trace status rows n landed
  pushed=$(probe push "$door_port" "$token" trace gate)
  status=$(printf '%s' "$pushed" | cut -d' ' -f1)
  trace=$(printf '%s' "$pushed" | cut -d' ' -f2)
  say "a gate trace answered $status as $trace"
  [ "$status" = 200 ] || return 1
  probe push "$door_port" "$token" metric gate >/dev/null
  n=0
  landed=""
  while [ "$n" -lt 45 ]; do
    landed=$(probe tempo-trace "$tempo_url" "$trace" 2>/dev/null || true)
    if [ "$landed" = gate ]; then break; fi
    landed=""
    n=$((n + 1))
    sleep 2
  done
  say "the gate trace landed in tempo as tagma.id=${landed:--}"
  [ "$landed" = gate ] || return 1
  wait_prom 'count({__name__=~"acceptance_probe_.*",tagma_id="gate"})' 90 || return 1
  rows=$(probe prom "$prom_url" 'count({__name__=~"acceptance_probe_.*",tagma_id="gate"})')
  say "the gate metric landed in prometheus: $rows"
  [ -n "$rows" ]
}

# Two processes, two identities, one door: each instance's constant gauge
# arrives under its own tagma.id, and its own service_instance.id beside it.
check3() {
  local rows ids
  wait_prom 'kallipai_tagma_build_info{tagma_id="'"${slugs[0]}"'"}' 180 || return 1
  wait_prom 'kallipai_tagma_build_info{tagma_id="'"${slugs[1]}"'"}' 180 || return 1
  rows=$(probe prom "$prom_url" 'count by (tagma_id) (kallipai_tagma_build_info)')
  ids=$(ids_from_prom 'kallipai_tagma_build_info')
  say "count by tagma_id: $(printf '%s' "$rows" | tr '\n' ';')"
  [ "$ids" = "$(printf '%s\n%s\n' "${slugs[0]}" "${slugs[1]}" | sort)" ]
}

# The two identity keys a process reports come from one stored entry, so a
# series carrying one instance's tagma.id beside the other's
# service_instance.id would mean the two lookups disagreed. Counting the
# instances by tagma_id alone and by both keys answers the same number only
# when each instance reports one identity.
check4() {
  local slug by_tagma by_both
  for slug in "${slugs[@]}"; do
    wait_prom 'kallipai_tagma_build_info{tagma_id="'"$slug"'",service_instance_id="'"$slug"'"}' 180 || return 1
    say "$slug: one series carries tagma_id and service_instance_id as itself"
  done
  by_tagma=$(value_of 'count(count by (tagma_id) (kallipai_tagma_build_info))') || return 1
  by_both=$(value_of 'count(count by (tagma_id, service_instance_id) (kallipai_tagma_build_info))') || return 1
  say "instances by tagma_id: ${by_tagma:-none}; by both keys: ${by_both:-none}"
  [ -n "$by_tagma" ] || return 1
  [ "$(printf '%s' "$by_tagma" | cut -d. -f1)" = "$(printf '%s' "$by_both" | cut -d. -f1)" ]
}

# Traffic has to be visible on the wire: each instance's request counter grows
# between two exports, which is what a dashboard reads.
check5() {
  local slug baseline base1 n grown
  for slug in "${slugs[@]}"; do
    hit "${tagma_ports[0]}"
    hit "${tagma_ports[1]}"
  done
  local expr0='sum(kallipai_tagma_http_requests_total{tagma_id="'"${slugs[0]}"'"})'
  local expr1='sum(kallipai_tagma_http_requests_total{tagma_id="'"${slugs[1]}"'"})'
  baseline=$(wait_value "$expr0" 180)
  say "baseline of ${slugs[0]}: ${baseline:-none}"
  [ -n "$baseline" ] || return 1
  n=0
  while [ "$n" -lt 6 ]; do
    hit "${tagma_ports[0]}"
    hit "${tagma_ports[1]}"
    n=$((n + 1))
    sleep 5
  done
  grown=$(wait_growth "$expr0" "$baseline" 240)
  say "${slugs[0]} after traffic: ${grown:--}"
  [ -n "$grown" ] || return 1
  base1=$(value_of "$expr1")
  [ -n "$base1" ] || return 1
  [ -n "$(wait_growth "$expr1" "$base1" 240)" ]
}

# The traces each process exports carry the same two keys, so a trace in the
# store can be attributed to the instance that produced it.
check6() {
  local rows slug found n
  hit "${tagma_ports[0]}"
  hit "${tagma_ports[1]}"
  n=0
  found=0
  while [ "$n" -lt 30 ]; do
    rows=$(tempo_rows)
    found=0
    for slug in "${slugs[@]}"; do
      if printf '%s\n' "$rows" | python3 -c 'import sys
slug = sys.argv[1]
for line in sys.stdin:
    parts = line.split()
    if len(parts) >= 4 and parts[1] == slug and parts[2] == slug:
        raise SystemExit(0)
raise SystemExit(1)' "$slug"; then
        found=$((found + 1))
      fi
    done
    [ "$found" = 2 ] && break
    n=$((n + 1))
    sleep 2
  done
  for slug in "${slugs[@]}"; do
    say "$slug: a trace carries service.instance.id=$slug and tagma.id=$slug (scan $n)"
  done
  [ "$found" = 2 ]
}

# One line per check in the report, so a FAIL names the claim that broke
# instead of leaving an operator to guess which face went wrong.
titles=(
  "the door takes the platform token and refuses anything else"
  "one push with tagma.id reaches the trace store and the metrics store"
  "two processes arrive under two tagma_id values through one door"
  "each instance reports one identity, not two that disagree"
  "each instance's request counter grows with traffic"
  "each instance's traces carry both of its identity keys"
)
for n in 1 2 3 4 5 6; do
  selected "$n" || continue
  report "$n" "${titles[$((n - 1))]}" "check$n"
done

if [ "${#failed[@]}" -gt 0 ]; then
  say "failed checks: ${failed[*]}"
  exit 1
fi
say "all selected checks passed"
