#!/usr/bin/env bash
# leakcheck.sh — fail when a fixture left an instance behind in its sandbox.
#
# Usage: scripts/leakcheck.sh --root <dir> [--root <dir>]... --window-start <ticks>
#
# `--window-start` is a start time in clock ticks since boot, the unit of field
# 22 of `/proc/<pid>/stat` (and of `CLOCK_BOOTTIME * CLK_TCK`). A process whose
# start time is earlier is not this run's business and is skipped.
#
# The evidence is the records a fixture writes, and only that:
#   * rule 1 — a `runtime.json` under a root names a pid that is still alive with
#     the start time it recorded -> leak, printed with the record and the pid,
#     exit 1. Every instance a fixture launches registers one, so this is the
#     rule that catches the leak shape (`setsid` in the launch helper leaves the
#     instance running when its daemon dies).
#   * rule 2 — a record under a root whose data, config or state root leaves the
#     roots -> leak. The daemon writes these before it launches, so a record is
#     the earliest written evidence of a foreign tree. Every JSON is read, not
#     only the ones shaped like an instance record: a description of a foreign
#     tree in the sandbox is a red we accept, because the cost of a false alarm
#     is a look at one file while the cost of a miss is the incident again.
#
# Process environments are deliberately not evidence. This host runs with
# `ptrace_scope=1`, where `/proc/<pid>/environ` of anything that is not one's own
# descendant is unreadable, so an environment scan reports `leak=0` on a machine
# full of leftovers: it looks like coverage and is none. A record with no start
# time, or one recorded as 0, is likewise not evidence — it cannot tell which
# incarnation of a pid it means.
#
# Output: one line per finding, then `leak=0` and exit 0 when clean. Exit 2 means
# the checker could not run (missing `python3`, bad arguments), never "clean".
set -uo pipefail

usage() {
  cat <<'USAGE'
usage: leakcheck.sh --root <dir> [--root <dir>]... --window-start <ticks>
  --root <dir>          sandbox root a leaked process must fall under (repeatable,
                        must be an absolute path below /)
  --window-start <ticks> start time in clock ticks since boot; earlier processes
                        are skipped. Compute as
                        awk -v hz=$(getconf CLK_TCK) '{printf "%d", int($1*hz)}' /proc/uptime
  exits 0 with `leak=0` when clean; prints each finding and exits 1; exits 2 when
  it cannot run at all (bad argument, missing python3)

The gate calls this from scripts/rusttest-gate.sh, which owns the one sandbox the
tests and this check share.
USAGE
}

die_usage() {
  echo "leakcheck: $1" >&2
  exit 2
}

ROOTS=()
WINDOW=""
while [ $# -gt 0 ]; do
  case "$1" in
    --root)
      [ $# -ge 2 ] || die_usage "--root needs a directory"
      root="${2%/}"
      case "$root" in
        '' | /) die_usage "--root $2 is not a sandbox: it would match every path" ;;
        /*) ROOTS+=("$root") ;;
        *) die_usage "--root $2 is not an absolute path" ;;
      esac
      shift 2
      ;;
    --window-start)
      [ $# -ge 2 ] || die_usage "--window-start needs a tick count"
      WINDOW="$2"
      shift 2
      ;;
    -h | --help) usage; exit 0 ;;
    *) die_usage "unknown argument: $1" ;;
  esac
done
[ ${#ROOTS[@]} -gt 0 ] || die_usage "at least one --root is required"
case "$WINDOW" in
  '' | *[!0-9]*) die_usage "--window-start must be a tick count" ;;
esac
# Every rule reads JSON, so a missing reader would turn the whole check into a
# silent pass.
command -v python3 >/dev/null 2>&1 || die_usage "python3 is required to read the records"

STARTTIME_OF() {
  local stat rest
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  rest=${stat##*')'}
  local -a fields
  read -r -a fields <<<"$rest"
  [ ${#fields[@]} -ge 20 ] || return 1
  case "${fields[0]}" in Z* | X*) return 1 ;; esac
  echo "${fields[19]}"
}

cmdline_of() {
  tr '\0' ' ' <"/proc/$1/cmdline" 2>/dev/null
}

FOUND=0

# Rule 1: a live instance named by a record inside the sandbox.
for root in "${ROOTS[@]}"; do
  [ -d "$root" ] || continue
  while IFS= read -r record; do
    read -r pid recorded < <(python3 -c '
import json, sys
record = json.load(open(sys.argv[1]))
pid = record.get("pid")
starttime = record.get("starttime")
print(pid if isinstance(pid, int) else "", starttime if isinstance(starttime, int) else "")
' "$record" 2>/dev/null) || continue
    case "$pid" in '' | *[!0-9]*) continue ;; esac
    # No pin, or a pin of 0, says nothing about which incarnation this is.
    case "$recorded" in '' | 0 | *[!0-9]*) continue ;; esac
    actual=$(STARTTIME_OF "$pid") || continue
    [ "$actual" = "$recorded" ] || continue
    [ "$actual" -ge "$WINDOW" ] || continue
    printf 'leak pid=%s record=%s starttime=%s cmd=%s\n' "$pid" "$record" "$actual" "$(cmdline_of "$pid")"
    FOUND=1
  done < <(find "$root" -name runtime.json -type f 2>/dev/null)
done

# Rule 2: a record under a root with a root of its own that leaves the roots.
for root in "${ROOTS[@]}"; do
  [ -d "$root" ] || continue
  while IFS= read -r record; do
    while IFS= read -r value; do
      [ -n "$value" ] || continue
      # Resolve before comparing: `<root>/../escaped` is outside, and a link
      # inside the sandbox is wherever it points.
      resolved=$(realpath -m -- "$value" 2>/dev/null || printf '%s' "$value")
      inside=0
      for candidate in "${ROOTS[@]}"; do
        root_real=$(realpath -m -- "$candidate" 2>/dev/null || printf '%s' "$candidate")
        case "$resolved" in "$root_real" | "$root_real"/*) inside=1 ;; esac
      done
      if [ "$inside" -eq 0 ]; then
        printf 'leak record=%s root=%s outside=%s\n' "$record" "$value" "${ROOTS[*]}"
        FOUND=1
      fi
    done < <(python3 -c '
import json, sys
record = json.load(open(sys.argv[1]))
for key in ("data_dir", "config_dir", "state_dir"):
    value = record.get(key)
    if isinstance(value, str) and value:
        print(value)
' "$record" 2>/dev/null)
  done < <(find "$root" -maxdepth 8 -name '*.json' -type f 2>/dev/null)
done

if [ "$FOUND" -ne 0 ]; then
  echo "leak=1"
  exit 1
fi
echo "leak=0"
exit 0
