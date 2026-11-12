#!/usr/bin/env bash
# leakcheck-selftest.sh — prove scripts/leakcheck.sh still can, and still does
# not, accuse.
#
# Usage: scripts/leakcheck-selftest.sh
#
# Each case builds its own throwaway sandbox and runs the checker over it. The
# red case plants the leak shape the fixtures produce — a live process named by a
# `runtime.json` that carries its pid and start time — and the checker has to name
# that pid. The green cases show the guard does not fire on a clean tree, on a
# record with no usable pin, or on a root it must refuse. Exit 0 when every case
# answers as expected.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
check="$here/leakcheck.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

failures=0

report() {
  printf 'selftest %s: %s\n' "$1" "$2"
  [ "$2" = ok ] || failures=$((failures + 1))
}

ticks_now() {
  local uptime hz
  read -r uptime _ </proc/uptime
  hz=$(getconf CLK_TCK)
  awk -v up="$uptime" -v hz="$hz" 'BEGIN { printf "%d", int(up * hz) }'
}

starttime_of() {
  local stat rest
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  rest=${stat##*')'}
  local -a fields
  read -r -a fields <<<"$rest"
  echo "${fields[19]}"
}

# Green: an empty sandbox has nothing to accuse.
clean="$scratch/clean"
mkdir -p "$clean"
out=$("$check" --root "$clean" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report clean-tree ok
else
  report clean-tree "expected leak=0 rc=0, got rc=$rc out=$out"
fi

# Red: a live process named by a record inside the sandbox.
leaky="$scratch/leaky"
mkdir -p "$leaky/state"
window=$(ticks_now)
setsid sleep 300 >/dev/null 2>&1 &
pid=$!
sleep 0.2
printf '{"pid": %d, "starttime": %s}\n' "$pid" "$(starttime_of "$pid")" >"$leaky/state/runtime.json"
out=$("$check" --root "$leaky" --window-start "$window" 2>&1)
rc=$?
if [ "$rc" -eq 1 ] && printf '%s\n' "$out" | grep -q "^leak pid=$pid record="; then
  report live-record "ok"
else
  report live-record "expected leak=1 naming pid=$pid, got rc=$rc out=$out"
fi
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

# Green: a record whose pin is 0 cannot say which incarnation it means.
unpinned="$scratch/unpinned"
mkdir -p "$unpinned/state"
setsid sleep 300 >/dev/null 2>&1 &
pid=$!
sleep 0.2
printf '{"pid": %d, "starttime": 0}\n' "$pid" >"$unpinned/state/runtime.json"
out=$("$check" --root "$unpinned" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report unknown-pin ok
else
  report unknown-pin "expected leak=0 rc=0, got rc=$rc out=$out"
fi
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

# Red: a record whose own root leaves the sandbox.
foreign="$scratch/foreign"
mkdir -p "$foreign/state"
printf '{"identity": {"pid": 1, "starttime": 1}, "data_dir": "/home/someone/.local/share/kallipai/tagmata/team"}\n' \
  >"$foreign/state/instance.json"
out=$("$check" --root "$foreign" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 1 ] && printf '%s\n' "$out" | grep -q '^leak record='; then
  report foreign-root ok
else
  report foreign-root "expected leak=1 naming the record, got rc=$rc out=$out"
fi

# Red: a root that only looks inside until it is resolved.
escape="$scratch/escape"
mkdir -p "$escape/inner" "$scratch/outside"
printf '{"identity": {"pid": 1}, "config_dir": "%s/escape/inner/../../outside"}\n' "$scratch" \
  >"$escape/inner/instance.json"
out=$("$check" --root "$escape" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 1 ]; then
  report resolved-root ok
else
  report resolved-root "expected leak=1 for a .. escape, got rc=$rc out=$out"
fi

# Red by design: any JSON naming a root outside the sandbox is a finding.
stray="$scratch/stray"
mkdir -p "$stray/state"
printf '{"data_dir": "/home/someone/tagmata/team"}\n' >"$stray/state/notes.json"
out=$("$check" --root "$stray" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 1 ]; then
  report foreign-note ok
else
  report foreign-note "expected leak=1 for a foreign root, got rc=$rc out=$out"
fi

# Red: a sibling whose name only shares a prefix is not inside the root, so a
# record naming it is a record naming a foreign root.
sibling="$scratch/sibling"
mkdir -p "$sibling/state" "${sibling}-2"
printf '{"identity": {"pid": 1}, "state_dir": "%s-2/state"}\n' "$sibling" \
  >"$sibling/state/instance.json"
out=$("$check" --root "$sibling" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 1 ]; then
  report prefix-sibling ok
else
  report prefix-sibling "expected leak=1 for a prefix sibling, got rc=$rc out=$out"
fi

# Green: a process older than the window is not this run's business.
early="$scratch/early"
mkdir -p "$early/state"
setsid sleep 300 >/dev/null 2>&1 &
pid=$!
sleep 0.2
printf '{"pid": %d, "starttime": %s}\n' "$pid" "$(starttime_of "$pid")" >"$early/state/runtime.json"
out=$("$check" --root "$early" --window-start "$(($(ticks_now) + 100000))" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report before-window ok
else
  report before-window "expected leak=0 rc=0, got rc=$rc out=$out"
fi
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

# Refusals: a root that would match everything, and a missing reader.
out=$("$check" --root / --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 2 ]; then
  report root-slash ok
else
  report root-slash "expected rc=2 for --root /, got rc=$rc out=$out"
fi

shim="$scratch/shimbin"
mkdir -p "$shim"
for tool in bash cat tr find awk getconf realpath; do
  ln -s "$(command -v "$tool")" "$shim/$tool"
done
out=$(PATH="$shim" "$check" --root "$clean" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 2 ]; then
  report no-python3 ok
else
  report no-python3 "expected rc=2 without python3, got rc=$rc out=$out"
fi

if [ "$failures" -ne 0 ]; then
  echo "selftest: $failures case(s) failed"
  exit 1
fi
echo "selftest: all cases ok"
exit 0
