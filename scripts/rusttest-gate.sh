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
env HOME="$sandbox/home" CARGO_HOME="$cargo_home" \
  XDG_DATA_HOME="$sandbox/data" XDG_CONFIG_HOME="$sandbox/config" \
  XDG_STATE_HOME="$sandbox/state" XDG_CACHE_HOME="$sandbox/cache" \
  TMPDIR="$sandbox/tmp" \
  cargo test -p kallipai-tagma --features sandbox-test
sandbox_suite=$?
echo "sandbox-test suite rc=$sandbox_suite"

"$here/leakcheck.sh" --root "$sandbox" --window-start "$window"
leak=$?
echo "leakcheck rc=$leak"

[ "$selftest" -eq 0 ] && [ "$tests" -eq 0 ] && [ "$sandbox_suite" -eq 0 ] && [ "$leak" -eq 0 ]
