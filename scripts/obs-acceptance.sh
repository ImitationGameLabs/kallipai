#!/usr/bin/env bash
# Acceptance for the telemetry intake: two tagma instances, their own doors,
# and the stores behind them. Each check reads a store, so a config that parses
# but drops the wrong thing fails here instead of in a dashboard.
#
#   scripts/obs-acceptance.sh [--keep] [--only 3,5] [--root DIR]
#
# Three prepared steps run before the numbered checks: the render gate (one
# rendered root, six refusals and one argument count through
# scripts/render-intake.py), the stack build with its three config validations
# against the pinned images, and the empty-table build. They are prerequisites,
# not checks: the first one that fails ends the run with its own message, so
# `--only` skips none of them and the "every check reports and the run carries
# on" rule does not apply to them.
#
# The run brings up tempo, prometheus and the collector under its own compose
# project, starts two `kallipai-tagma` processes from this checkout's build,
# and pushes OTLP through the rendered texts. Ports 12000/12001 (intakes),
# 7301/7302 (the instances' management faces), 3200 (tempo), 19090 (prometheus)
# and 13133 (collector health) must be free: stop the dev stack first.
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
door_ports=(12000 12001)
metrics_ports=(7301 7302)
prom_port=19090
tempo_port=3200
health_port=13133
keep=0
only=""
root=""

usage() {
  sed -n '2,27p' "${BASH_SOURCE[0]}" >&2
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
telemetry=$root/telemetry
table=$root/telemetry-instances.json
stack_dir=$root/stack

say "root $root"
mkdir -p "$telemetry/gateway/tokens" "$root/instances" "$root/logs" "$root/run"

# One table, read by the stack build and by the instances themselves, so a port
# and a token path are written once.
python3 - "$table" "$telemetry" "$root" "${slugs[0]}" "${slugs[1]}" "${door_ports[0]}" "${door_ports[1]}" "${metrics_ports[0]}" "${metrics_ports[1]}" <<'PY'
import json
import sys
table, telemetry, root, a, b, da, db, ma, mb = sys.argv[1:10]
rows = [
    {"slug": slug, "otlpPort": int(door), "metricsPort": int(metrics),
     "dataRoot": "%s/instances/%s" % (root, slug),
     "tokenPath": "%s/gateway/tokens/%s" % (telemetry, slug)}
    for slug, door, metrics in ((a, da, ma), (b, db, mb))
]
open(table, "w").write(json.dumps(rows, indent=2) + "\n")
PY

token_a=$(python3 -c 'import secrets; print("acc-a-" + secrets.token_hex(16))')
token_b=$(python3 -c 'import secrets; print("acc-b-" + secrets.token_hex(16))')
printf '%s' "$token_a" >"$telemetry/gateway/tokens/${slugs[0]}"
printf '%s' "$token_b" >"$telemetry/gateway/tokens/${slugs[1]}"
chmod 600 "$telemetry/gateway/tokens/"*
# An instance stores its enrollment as one relay entry: credentials/<entry>/
# under the data root, named by the [[polis]] entry in polis.toml. The url is
# never dialled here, it only has to parse.
for i in 0 1; do
  slug=${slugs[$i]}
  data=$root/instances/$slug
  mkdir -p "$data/credentials/acc"
  printf '[[polis]]\nname = "acc"\nurl = "http://127.0.0.1:9"\n' >"$data/polis.toml"
  printf '%s' "$slug" >"$data/credentials/acc/tagma.id"
  if [ "$i" = 0 ]; then printf '%s' "$token_a" >"$data/credentials/acc/tagma.token"; else printf '%s' "$token_b" >"$data/credentials/acc/tagma.token"; fi
  chmod 600 "$data/credentials/acc/tagma.id" "$data/credentials/acc/tagma.token"
done
# The collector runs as root inside its container and reads the token files by
# their host paths, so the path leading to them has to be traversable.
chmod a+rX "$root" "$telemetry" "$telemetry/gateway" "$telemetry/gateway/tokens"

say "the render step: one rendered root, six refusals and one argument count"
# The deployment step nothing else runs: synthetic data roots, one real run
# each, and the outcome an operator would get. Every refusal the renderer
# promises is walked here, so a guard that stops guarding fails the run.
render_gate() {
  local base=$root/render
  local renderer=$repo_root/scripts/render-intake.py
  local session got mode
  for session in one zero two escape control empty missing; do
    mkdir -p "$base/$session/credentials/acc" "$base/$session/gateway"
  done
  mkdir -p "$base/two/credentials/other"
  printf 'tok-one\n' >"$base/one/credentials/acc/tagma.token"
  printf 'tok-a\n' >"$base/two/credentials/acc/tagma.token"
  printf 'tok-b\n' >"$base/two/credentials/other/tagma.token"
  printf 'tok-esc\n' >"$base/escape/credentials/acc/tagma.token"
  printf 'tok\001ctl\n' >"$base/control/credentials/acc/tagma.token"
  printf '   \n' >"$base/empty/credentials/acc/tagma.token"
  printf 'tok-missing\n' >"$base/missing/credentials/acc/tagma.token"
  printf 'agent: text\n' >"$base/agent.yaml"
  for session in one zero two escape control empty missing; do
    printf '[{"slug":"acc-a","otlpPort":12000,"metricsPort":7301,"dataRoot":"%s","tokenPath":"%s"}]\n' \
      "$base/$session" "$base/$session/gateway/tokens/acc-a" >"$base/$session/table.json"
  done
  # The path resolves through `..` out of the mounted directory: the guard has
  # to read the real path, not the spelling.
  printf '[{"slug":"acc-a","otlpPort":12000,"metricsPort":7301,"dataRoot":"%s","tokenPath":"%s/gateway/tokens/../../elsewhere/acc-a"}]\n' \
    "$base/escape" "$base/escape" >"$base/escape/table.json"
  # A row that carries no dataRoot at all.
  printf '[{"slug":"acc-a","otlpPort":12000,"metricsPort":7301,"tokenPath":"%s/gateway/tokens/acc-a"}]\n' \
    "$base/missing" >"$base/missing/table.json"

  python3 "$renderer" "$base/one/table.json" "$base/one" "$base" || return 1
  got=$(cat "$base/one/gateway/tokens/acc-a" 2>/dev/null || true)
  [ "$got" = tok-one ] || return 1
  # The token is naked to root alone and the agent text to nobody: a mode that
  # drifts to 0644 hands every credential on the host to every local account.
  [ "$(stat -c %a "$base/one/gateway/tokens/acc-a")" = 600 ] || return 1
  [ "$(stat -c %a "$base/one/agent.yaml")" = 444 ] || return 1

  python3 "$renderer" "$base/zero/table.json" "$base/zero" "$base" 2>"$base/zero.err" && return 1
  grep -q 'has 0 credentials' "$base/zero.err" || return 1
  grep -q '^render-intake: ' "$base/zero.err" || return 1
  python3 "$renderer" "$base/two/table.json" "$base/two" "$base" 2>"$base/two.err" && return 1
  grep -q 'has 2 credentials' "$base/two.err" || return 1
  grep -q 'boot that instance once' "$base/two.err" || return 1

  for mode in escape control empty missing; do
    python3 "$renderer" "$base/$mode/table.json" "$base/$mode" "$base" 2>"$base/$mode.err" && return 1
  done
  grep -q 'writes its token outside' "$base/escape.err" || return 1
  grep -q 'control character' "$base/control.err" || return 1
  grep -q 'is empty once trimmed' "$base/empty.err" || return 1
  grep -q "an instance has no 'dataRoot'" "$base/missing.err" || return 1

  # A wrong argument count is the other exit: it must be a usage line and 2,
  # not a Python traceback.
  python3 "$renderer" "$base/one/table.json" 2>"$base/args.err" && return 1
  grep -q '^render-intake: usage: ' "$base/args.err"
}
render_gate || fail "the render step did not answer its data roots as documented"

say "rendering the stack texts"
stack_dir=$(cd "$repo_root" && KALLIPAI_TELEMETRY_INSTANCES_FILE=$table \
  nix build .#kallipai-stack --impure --no-link --print-out-paths)
[ -f "$stack_dir/otelcol.yaml" ] || fail "the stack build wrote no collector text"
[ -f "$stack_dir/agent.yaml" ] || fail "the stack build wrote no agent text although the table lists two instances"

# Every rendered text is validated by the image that runs it before anything
# starts: a rejected config fails here with the collector's own message.
say "validating the rendered texts against the pinned image"
collector_image=$(image_of otelcol)
docker run --rm --user "0:0" -v "$stack_dir/otelcol.yaml:/cfg.yaml:ro" \
  -v "$telemetry/gateway:$telemetry/gateway:ro" "$collector_image" validate --config=/cfg.yaml
docker run --rm --user "0:0" -v "$stack_dir/agent.yaml:/cfg.yaml:ro" \
  -v "$telemetry/gateway:$telemetry/gateway:ro" "$collector_image" validate --config=/cfg.yaml
empty=$root/empty-instances.json
printf '[]\n' >"$empty"
plain=$(cd "$repo_root" && KALLIPAI_TELEMETRY_INSTANCES_FILE=$empty \
  nix build .#kallipai-stack --impure --no-link --print-out-paths)
docker run --rm --user "0:0" -v "$plain/otelcol.yaml:/cfg.yaml:ro" "$collector_image" validate --config=/cfg.yaml
say "all three configs validate"

# --- the stack ----------------------------------------------------------------
mkdir -p "$root/prom" "$root/tempo"
chmod 777 "$root/prom"
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
    ports: ["127.0.0.1:$tempo_port:$tempo_port"]
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
    image: $(image_of otelcol)
    user: "0:0"
    depends_on: [tempo]
    volumes:
      - $stack_dir/otelcol.yaml:/etc/otelcol-contrib/config.yaml:ro
      - $telemetry/gateway:$telemetry/gateway:ro
    ports:
      - "127.0.0.1:${door_ports[0]}:${door_ports[0]}"
      - "127.0.0.1:${door_ports[1]}:${door_ports[1]}"
      - "127.0.0.1:$health_port:$health_port"
YAML

# The agent reads every instance's management face over loopback, so it shares
# the host's network namespace. This mirrors scripts/compose-up.sh, which owns
# the container in a deployed stack; the arguments are the ones it uses.
agent=kallipai-obs-acceptance-agent
start_agent() {
  docker rm -f "$agent" >/dev/null 2>&1 || true
  docker run --detach --name "$agent" --network host --restart no --user "0:0" \
    --volume "$stack_dir/agent.yaml:/etc/otelcol-contrib/config.yaml:ro" \
    --volume "$telemetry/gateway:$telemetry/gateway:ro" \
    "$collector_image" >/dev/null
}

declare -a tagma_pids=()
start_instance() {
  local i=$1
  local slug=${slugs[$i]}
  local data=$root/instances/$slug
  local log=$root/logs/$slug.log
  KALLIPAI_TAGMA_SLUG=$slug \
    KALLIPAI_TAGMA_DATA_DIR=$data \
    KALLIPAI_TAGMA_ADDR=127.0.0.1:${metrics_ports[$i]} \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:${door_ports[$i]} \
    OTEL_RESOURCE_ATTRIBUTES="service.instance.id=$slug" \
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
  for i in 0 1; do stop_instance "$i"; done
  docker rm -f "$agent" >/dev/null 2>&1 || true
  docker compose -p "$project" -f "$root/compose.yaml" down -v >/dev/null 2>&1 || true
  if [ "$status" = 0 ] && [ "$keep" = 0 ]; then
    # The pruner's directories come back owned by the container's user, so a
    # plain rm can be refused after a run that otherwise succeeded.
    chmod -R u+rwX "$root" 2>/dev/null || true
    rm -rf "$root" 2>/dev/null || true
  else
    say "kept $root (logs in $root/logs, run state in $root/run)"
  fi
  exit "$status"
}
trap teardown EXIT

say "starting tempo, prometheus and the collector"
docker compose -p "$project" -f "$root/compose.yaml" up -d >/dev/null

say "building kallipai-tagma"
tagma_bin=$(cd "$repo_root" && nix build .#packages.x86_64-linux.kallipai-tagma --no-link --print-out-paths)/bin/kallipai-tagma
[ -x "$tagma_bin" ] || fail "the build produced no kallipai-tagma"

say "starting ${slugs[*]}"
: >"$root/run/instances"
start_instance 0
start_instance 1
start_agent


probe() { python3 "$repo_root/scripts/obs-acceptance-probe.py" "$@"; }
prom_url=http://127.0.0.1:$prom_port
tempo_url=http://127.0.0.1:$tempo_port
scrape_wait=50

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
    python3 -c 'import re,sys; print("\n".join(sorted({m for line in sys.stdin for m in re.findall(r"tagma_id=([^,]*)", line) if m})))'
}
tempo_rows() { probe tempo "$tempo_url"; }

# How many traces Tempo holds under one resource tagma.id: the baseline a
# claim has to leave untouched.
tempo_count() {
  probe tempo-count "$tempo_url" "$1"
}

# A check run alone still has to wait for the first scrape: the agent scrapes
# every 15 seconds and the pipeline adds a batch interval on top.
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

check1() {
  probe wait "http://127.0.0.1:$health_port/" 90 | grep -qx ready || return 1
  local i
  for i in 0 1; do
    probe wait "http://127.0.0.1:${metrics_ports[$i]}/metrics" 60 | grep -qx ready || return 1
    local pushed
    pushed=$(probe push "${door_ports[$i]}" "$([ "$i" = 0 ] && printf '%s' "$token_a" || printf '%s' "$token_b")" trace "${slugs[$i]}")
    say "instance ${slugs[$i]} door ${door_ports[$i]} answered $(printf '%s' "$pushed" | cut -d' ' -f1)"
    [ "$(printf '%s' "$pushed" | cut -d' ' -f1)" = 200 ] || return 1
  done
  # The door refuses a token it does not know: without this the tenant would be
  # whatever the client says it is.
  local refused
  refused=$(probe push "${door_ports[0]}" "not-${token_a}" trace "${slugs[1]}")
  say "a foreign token on door ${door_ports[0]} answered $(printf '%s' "$refused" | cut -d' ' -f1)"
  [ "$(printf '%s' "$refused" | cut -d' ' -f1)" = 401 ]
}

check2() {
  hit "${metrics_ports[0]}"
  hit "${metrics_ports[1]}"
  wait_prom 'up{tagma_id="'"${slugs[0]}"'"}' 90 || return 1
  sleep 5
  local up_ids tempo_ids
  up_ids=$(ids_from_prom 'up')
  tempo_ids=$(tempo_rows | python3 -c 'import sys; print("\n".join(sorted({p[1] for p in (l.split() for l in sys.stdin) if len(p) > 2 and p[1] == p[2] and p[1] != "-"})))')
  say "up tagma_id: $(printf '%s' "$up_ids" | tr '\n' ' ')"
  say "tempo tagma.id: $(printf '%s' "$tempo_ids" | tr '\n' ' ')"
  [ -n "$up_ids" ] && [ "$up_ids" = "$tempo_ids" ]
}

check3() {
  local rows n
  n=0
  while [ "$n" -lt 45 ]; do
    rows=$(probe prom "$prom_url" 'count by (tagma_id) (kallipai_tagma_build_info)')
    if [ "$(printf '%s\n' "$rows" | grep -c ' 1$')" = 2 ]; then break; fi
    n=$((n + 1))
    sleep 2
  done
  say "count by tagma_id: $(printf '%s' "$rows" | tr '\n' ';')"
  [ "$(printf '%s\n' "$rows" | grep -c ' 1$')" = 2 ]
}

check4() {
  local rows slug found=0
  rows=$(tempo_rows)
  for slug in "${slugs[@]}"; do
    if printf '%s\n' "$rows" | python3 -c 'import sys
slug = sys.argv[1]
for line in sys.stdin:
    parts = line.split()
    if len(parts) >= 4 and parts[1] == slug and parts[2] == slug:
        raise SystemExit(0)
raise SystemExit(1)' "$slug"; then
      say "$slug: a trace carries service.instance.id=$slug and tagma.id=$slug"
      found=$((found + 1))
    else
      say "$slug: no trace carries both ids"
    fi
  done
  [ "$found" = 2 ] || return 1
  # The name in the store is the door's, not the sender's: a real token on
  # acc-a's door that claims acc-b in its resource attributes has to land as
  # acc-a, and acc-b's count has to stay where it was.
  local baseline claimed probe_trace n=0 landed
  # Only this run's own probes count: a slug the store already holds a hundred
  # traces for would make an absolute count equal on both sides for free.
  local probe_expr="{ resource.tagma.id = \"${slugs[1]}\" && resource.service.name = \"acceptance\" }"
  baseline=$(tempo_count "$probe_expr")
  claimed=$(probe push "${door_ports[0]}" "$token_a" trace "${slugs[1]}")
  [ "$(printf '%s' "$claimed" | cut -d' ' -f1)" = 200 ] || return 1
  probe_trace=$(printf '%s' "$claimed" | cut -d' ' -f2)
  while [ "$n" -lt 60 ]; do
    if landed=$(probe tempo-trace "$tempo_url" "$probe_trace" 2>/dev/null); then break; fi
    landed=""
    n=$((n + 1))
    sleep 2
  done
  say "a claim of ${slugs[1]} on door ${door_ports[0]} landed as ${landed:--}"
  [ "$landed" = "${slugs[0]}" ] || return 1
  [ "$(tempo_count "$probe_expr")" = "$baseline" ]
}

# One broken credential: the refused export is counted, the accepted one is not,
# that instance's traces stop, and the other instance keeps exporting.
check5() {
  local started bad
  started=$(date +%s%N)
  bad="wrong-${token_b}"
  printf '%s' "$bad" >"$root/instances/${slugs[1]}/credentials/acc/tagma.token"
  chmod 600 "$root/instances/${slugs[1]}/credentials/acc/tagma.token"
  stop_instance 1
  start_instance 1
  probe wait "http://127.0.0.1:${metrics_ports[1]}/metrics" 60 | grep -qx ready || return 1
  hit "${metrics_ports[1]}"
  hit "${metrics_ports[0]}"
  sleep 25
  local refused exported_a exported_b stopped
  refused=$(probe prom "$prom_url" "kallipai_tagma_telemetry_export_failed_total{tagma_id=\"${slugs[1]}\",reason=\"unauthorized\"}")
  exported_b=$(probe prom "$prom_url" "kallipai_tagma_telemetry_exported_total{tagma_id=\"${slugs[1]}\"}")
  exported_a=$(probe prom "$prom_url" "kallipai_tagma_telemetry_exported_total{tagma_id=\"${slugs[0]}\"}")
  stopped=$(tempo_rows | python3 -c 'import sys
start = int(sys.argv[1])
slug = sys.argv[2]
new = 0
for line in sys.stdin:
    parts = line.split()
    if len(parts) >= 4 and parts[2] == slug and int(parts[0]) >= start:
        new += 1
print(new)' "$started" "${slugs[1]}")
  say "refused export of ${slugs[1]}: ${refused:-none}"
  say "accepted export of ${slugs[1]}: ${exported_b:-0}"
  say "accepted export of ${slugs[0]}: ${exported_a:-none}"
  say "traces from ${slugs[1]} after the change: $stopped"
  local refused_count
  refused_count=$(printf '%s' "$refused" | python3 -c 'import sys; v = sys.stdin.read().split(); print(v[-1] if v else 0)')
  [ "$(printf '%s' "$refused_count" | cut -d. -f1)" -gt 0 ] || return 1
  [ -z "$exported_b" ] || [ "$(printf '%s' "$exported_b" | cut -d' ' -f2)" = 0 ] || return 1
  [ -n "$exported_a" ] || return 1
  [ "$stopped" = 0 ]
}

# A stopped instance leaves the scrape with no target: two intervals later the
# series is zero, not stale.
check6() {
  stop_instance 0
  say "stopped ${slugs[0]}; waiting $scrape_wait seconds for two scrapes"
  sleep "$scrape_wait"
  local gone alive
  gone=$(probe prom "$prom_url" "up{tagma_id=\"${slugs[0]}\"}")
  alive=$(probe prom "$prom_url" "up{tagma_id=\"${slugs[1]}\"}")
  say "up ${slugs[0]}: ${gone:-no series}"
  say "up ${slugs[1]}: ${alive:-no series}"
  printf '%s' "$gone" | python3 -c 'import sys; v = sys.stdin.read().split(); raise SystemExit(0 if v and float(v[-1]) == 0 else 1)' || return 1
  # The other instance has to stay up: a scrape that lost both targets would
  # satisfy the check above for the wrong reason.
  printf '%s' "$alive" | python3 -c 'import sys; v = sys.stdin.read().split(); raise SystemExit(0 if v and float(v[-1]) == 1 else 1)'
}

# One line per check in the report, so a FAIL names the claim that broke
# instead of leaving an operator to guess which face of check 4 went wrong.
titles=(
  "the pipeline answers a real push and refuses a foreign token"
  "the scraped up series and the trace store agree on tagma_id"
  "each instance gets its own build_info series"
  "the name in the store is the door's, not the sender's"
  "a broken credential stops that instance and only that one"
  "a stopped instance leaves the scrape instead of going stale"
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
