#!/usr/bin/env bash
# rusttest-gate.sh — run the workspace tests inside one sandbox and then check
# that nothing survived them.
#
# Usage: scripts/rusttest-gate.sh [extra cargo test args...]
#
# The gate calls this and nothing else: one sandbox holds every fixture root
# (HOME, the four XDG homes, TMPDIR), so the leak check has a single place to
# look and a fixture that escapes is also a fixture lying about its roots. Both
# halves run even when the first fails, because a test failure must not hide a
# leaked process.
#
# The sandbox is printed and left on disk for inspection; TMPDIR points into it.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(dirname "$here")
sandbox=$(mktemp -d "${TMPDIR:-/tmp}/kallipai-rusttest-XXXXXX")
mkdir -p "$sandbox"/{home,data,config,state,cache,tmp}
cargo_home=${CARGO_HOME:-$HOME/.cargo}
hz=$(getconf CLK_TCK)
window=$(awk -v hz="$hz" '{printf "%d", int($1 * hz)}' /proc/uptime)

echo "rusttest-gate sandbox=$sandbox"
"$here/leakcheck-selftest.sh"
selftest=$?
echo "leakcheck-selftest rc=$selftest"
cd "$repo" || exit 2
env HOME="$sandbox/home" CARGO_HOME="$cargo_home" \
  XDG_DATA_HOME="$sandbox/data" XDG_CONFIG_HOME="$sandbox/config" \
  XDG_STATE_HOME="$sandbox/state" XDG_CACHE_HOME="$sandbox/cache" \
  TMPDIR="$sandbox/tmp" \
  cargo test --workspace "$@"
tests=$?
echo "cargo test rc=$tests"

# The sandbox suite is behind a feature, so the default workspace run never even
# compiles it. Compile it here and run it: on a host without the privileges the
# cases skip, but a guard that rejects its own root still fails to build or
# to pass, which is how that kind of mistake becomes visible.
#
# On such a host the lane is green after compiling and asserting nothing, so the
# harness prints its own count when the binary exits
# (`sandbox: summary ran=<R> skipped=<S> total=<T> expected=<E>`) and this gate
# quotes that line: the numbers come from the guard that turned the cases away,
# never from the text of the messages it printed. `E` is the scenario count
# declared in the harness, so `R + S` falling short of it means a scenario was
# added without asking the guard -- a check the line could not make on its own.
# The run's output still streams to the gate (tee), because that invocation
# covers more than the sandbox target: the tagma suites run a second time
# under the feature, and those `test result:` lines are part of what this lane
# ran (hiding them once cost the gate 1052 passing tests it never reported).
# Only a copy is kept for parsing the summary, and the last such line is the
# one the binary prints as it exits.
#
# `--nocapture` is for the skip reasons, not for the summary: the summary reaches
# the gate through `atexit` on the real stderr, while libtest captures what a
# passing test prints and throws it away -- so without the flag a skipped run
# would say how many cases it skipped and never why.
sandbox_out=$(mktemp "${TMPDIR:-/tmp}/rusttest-sandbox-XXXXXX")
env HOME="$sandbox/home" CARGO_HOME="$cargo_home" \
  XDG_DATA_HOME="$sandbox/data" XDG_CONFIG_HOME="$sandbox/config" \
  XDG_STATE_HOME="$sandbox/state" XDG_CACHE_HOME="$sandbox/cache" \
  TMPDIR="$sandbox/tmp" \
  cargo test -p kallipai-tagma --features sandbox-test -- --nocapture 2>&1 | tee "$sandbox_out"
sandbox_suite=${PIPESTATUS[0]}
echo "sandbox-test suite rc=$sandbox_suite"
sandbox_report=0
summary=$(grep '^sandbox: summary ' "$sandbox_out" | tail -n 1 || true)
if [ -z "$summary" ]; then
  echo "sandbox-test note=missing (the harness printed no 'sandbox: summary' line, so this lane cannot say whether its cases ran)"
  tail -n 20 "$sandbox_out"
  sandbox_report=1
else
  echo "$summary"
  if [[ $summary =~ ^sandbox:\ summary\ ran=([0-9]+)\ skipped=([0-9]+)\ total=([0-9]+)\ expected=([0-9]+)$ ]]; then
    expected=${BASH_REMATCH[4]}
    ran=${BASH_REMATCH[1]}
    skipped=${BASH_REMATCH[2]}
    total=${BASH_REMATCH[3]}
    if [ "$((ran + skipped))" -ne "$expected" ]; then
      echo "sandbox-test note=inconsistent ($summary: the guard ran $ran and turned away $skipped of $expected scenarios)"
      sandbox_report=1
    elif [ "$ran" -eq 0 ]; then
      echo "sandbox-test note=compiled-only ran=0 skipped=$skipped/$total scenarios (this host cannot run a sandboxed child; the cases were compiled and not executed)"
    else
      echo "sandbox-test note=executed ran=$ran/$total scenarios"
    fi
  else
    echo "sandbox-test note=unparsable ($summary)"
    sandbox_report=1
  fi
fi
rm -f "$sandbox_out"

"$here/leakcheck.sh" --root "$sandbox" --window-start "$window"
leak=$?
echo "leakcheck rc=$leak"

[ "$selftest" -eq 0 ] && [ "$tests" -eq 0 ] && [ "$sandbox_suite" -eq 0 ] && [ "$sandbox_report" -eq 0 ] && [ "$leak" -eq 0 ]
