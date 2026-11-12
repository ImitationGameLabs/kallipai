#!/usr/bin/env bash
# leakcheck-selftest.sh — prove scripts/leakcheck.sh still can, and still does
# not, accuse.
#
# Usage: scripts/leakcheck-selftest.sh
#
# Each case builds its own throwaway sandbox and runs the checker over it. The
# The cases plant the two shapes a fixture produces -- a live process named
# by a `runtime.json`, and a session whose recorded member died while its
# grandchild kept running -- and the checker has to name the right pids. The
# green cases show the guard does not fire on a clean tree, on a record with
# no usable pin, or on a root it must refuse, and one case pins the record
# that is undecidable because it has no session pin. Exit 0 when every case
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

# The first child of $1, waiting up to 2s for the fork to land. A fixed sleep is
# a race that reports a fake failure on a loaded machine, and these cases are
# the cases to copy when adding a session-shaped case.
first_child_of() {
  local leader=$1 i=0 child=""
  while [ "$i" -lt 40 ]; do
    child=$(cat "/proc/$leader/task/$leader/children" 2>/dev/null)
    child=${child%% *}
    [ -n "$child" ] && break
    sleep 0.05
    i=$((i + 1))
  done
  printf '%s' "$child"
}

# The kernel session id of $1, read the way the checker reads it.
sid_of() {
  local stat rest
  stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
  rest=${stat##*')'}
  local -a fields
  read -r -a fields <<<"$rest"
  echo "${fields[3]}"
}

# A session shaped the way the launch shapes one: the leader (recorded
# nowhere) starts a member, the member starts a child of its own, and all of
# them stay in the leader's session. Writes the three pids into $1 and, into
# $2, the record the instance would write: the member's pid, which is not the
# session id, plus that session id.
production_session() {
  local pids=$1 record=$2 leader member grandchild
  setsid sh -c "sh -c 'sleep 300 & wait' & wait" >/dev/null 2>&1 &
  leader=$!
  member=$(first_child_of "$leader")
  grandchild=$(first_child_of "$member")
  if [ -z "$member" ] || [ -z "$grandchild" ]; then
    kill -- "-$leader" 2>/dev/null
    return 1
  fi
  printf '%s' "$leader" >"$pids/leader"
  printf '%s' "$member" >"$pids/member"
  printf '%s' "$grandchild" >"$pids/grandchild"
  printf '{"pid": %d, "starttime": %s, "sid": %s}\n' "$member" \
    "$(starttime_of "$member")" "$(sid_of "$member")" >"$record"
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
printf '{"pid": %d, "starttime": %s, "sid": %s}\n' "$pid" "$(starttime_of "$pid")" "$(sid_of "$pid")" \
  >"$leaky/state/runtime.json"
out=$("$check" --root "$leaky" --window-start "$window" 2>&1)
rc=$?
if [ "$rc" -eq 1 ] && printf '%s\n' "$out" | grep -q "^leak pid=$pid record="; then
  report live-record ok
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

# Red: a record that carries a pid and a start time but no session pin. It is
# not clean, it is undecidable, and the checker says which.
unverifiable="$scratch/unverifiable"
mkdir -p "$unverifiable/state"
setsid sleep 300 >/dev/null 2>&1 &
pid=$!
sleep 0.2
printf '{"pid": %d, "starttime": %s}\n' "$pid" "$(starttime_of "$pid")" \
  >"$unverifiable/state/runtime.json"
out=$("$check" --root "$unverifiable" --window-start "$(ticks_now)" 2>&1)
rc=$?
if [ "$rc" -eq 1 ] && [ "$out" = "unverifiable record=$unverifiable/state/runtime.json pid=$pid reason=no-session-pin
leak=0 unverifiable=1" ]; then
  report no-session-pin ok
else
  report no-session-pin "expected unverifiable rc=1, got rc=$rc out=$out"
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
printf '{"pid": %d, "starttime": %s, "sid": %s}\n' "$pid" "$(starttime_of "$pid")" "$(sid_of "$pid")" \
  >"$early/state/runtime.json"
out=$("$check" --root "$early" --window-start "$(( $(ticks_now) + 100000 ))" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report before-window ok
else
  report before-window "expected leak=0 rc=0, got rc=$rc out=$out"
fi
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null

# Red: the shape the launch produces. A session leader forks the member, the
# member forks a child of its own; every process stays in the leader's session,
# and the record names the member, not the leader. The member dies and the
# grandchild keeps running: only the session branch can see it, and the record's
# own pid must not be named by that branch.
orphan="$scratch/orphan"
mkdir -p "$orphan/state" "$orphan/pids"
window=$(ticks_now)
production_session "$orphan/pids" "$orphan/state/runtime.json"
leader=$(cat "$orphan/pids/leader")
member=$(cat "$orphan/pids/member")
grandchild=$(cat "$orphan/pids/grandchild")
kill -9 "$member" 2>/dev/null
wait "$member" 2>/dev/null
# The leader is killed too, not left to notice its child is gone: a live
# leader is a member of the session and would be named, turning the count
# below into a race.
kill -9 "$leader" 2>/dev/null
wait "$leader" 2>/dev/null

sleep 0.1
out=$("$check" --root "$orphan" --window-start "$window" 2>&1)
rc=$?
named=$(printf '%s\n' "$out" | grep -c "^leak kind=session session=$leader pid=$grandchild ")
leaks=$(printf '%s\n' "$out" | grep -c '^leak ')
mentions_member=$(printf '%s\n' "$out" | grep -c "pid=$member ")
if [ "$rc" -eq 1 ] && [ "$named" -eq 1 ] && [ "$leaks" -eq 1 ] && [ "$mentions_member" -eq 0 ]; then
  report orphan-session ok
else
  report orphan-session "expected one session line naming leader=$leader grandchild=$grandchild and no line naming member=$member, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null

# Red: the member alive and the leader gone. The member is rule 1's line, the
# grandchild is the session branch's, and neither is named twice.
both="$scratch/both"
mkdir -p "$both/state" "$both/pids"
window=$(ticks_now)
production_session "$both/pids" "$both/state/runtime.json"
leader=$(cat "$both/pids/leader")
member=$(cat "$both/pids/member")
grandchild=$(cat "$both/pids/grandchild")

# The leader is a shell waiting on its member; kill it so only the member and
# its child are left, which is the shape of a session that outlived its leader.
kill -9 "$leader" 2>/dev/null
wait "$leader" 2>/dev/null
out=$("$check" --root "$both" --window-start "$window" 2>&1)
rc=$?
member_lines=$(printf '%s\n' "$out" | grep -c "^leak pid=$member record=")
grandchild_lines=$(printf '%s\n' "$out" | grep -c "^leak kind=session session=$leader pid=$grandchild ")
leaks=$(printf '%s\n' "$out" | grep -c '^leak ')
if [ "$rc" -eq 1 ] && [ "$member_lines" -eq 1 ] && [ "$grandchild_lines" -eq 1 ] && [ "$leaks" -eq 2 ]; then
  report live-session ok
else
  report live-session "expected one line for member=$member and one for grandchild=$grandchild, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null

# Red: a live leader stays in the report. It is a process in the session like
# any other, so a live leader is named -- three lines: rule 1's member and the
# session branch's leader and grandchild.
liveleader="$scratch/live-leader"
mkdir -p "$liveleader/state" "$liveleader/pids"
window=$(ticks_now)
production_session "$liveleader/pids" "$liveleader/state/runtime.json"
leader=$(cat "$liveleader/pids/leader")
member=$(cat "$liveleader/pids/member")
grandchild=$(cat "$liveleader/pids/grandchild")
out=$("$check" --root "$liveleader" --window-start "$window" 2>&1)
rc=$?
member_lines=$(printf '%s\n' "$out" | grep -c "^leak pid=$member record=")
leader_lines=$(printf '%s\n' "$out" | grep -c "^leak kind=session session=$leader pid=$leader ")
grandchild_lines=$(printf '%s\n' "$out" | grep -c "^leak kind=session session=$leader pid=$grandchild ")
leaks=$(printf '%s\n' "$out" | grep -c '^leak ')
if [ "$rc" -eq 1 ] && [ "$member_lines" -eq 1 ] && [ "$leader_lines" -eq 1 ] && [ "$grandchild_lines" -eq 1 ] && [ "$leaks" -eq 3 ]; then
  report live-leader-session ok
else
  report live-leader-session "expected member=$member, leader=$leader and grandchild=$grandchild named once each, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null

# Red only in the session branch: the pid is alive with another start time, so
# rule 1 must not name it -- but the session its record pins still speaks, and
# the member that started inside the window is named.
stale="$scratch/stale"
mkdir -p "$stale/state"
window=$(ticks_now)
setsid sh -c 'sleep 300 & wait' >/dev/null 2>&1 &
leader=$!
member=$(first_child_of "$leader")
sleep 0.2
printf '{"pid": %d, "starttime": %s, "sid": %s}\n' "$leader" \
  "$(( $(starttime_of "$leader") + 1 ))" "$(sid_of "$leader")" >"$stale/state/runtime.json"
out=$("$check" --root "$stale" --window-start "$window" 2>&1)
rc=$?
named_pid=$(printf '%s\n' "$out" | grep -c "^leak pid=$leader ")
named_member=$(printf '%s\n' "$out" | grep -c "^leak kind=session session=$leader pid=$member ")
if [ "$rc" -eq 1 ] && [ "$named_pid" -eq 0 ] && [ "$named_member" -eq 1 ]; then
  report stale-incarnation ok
else
  report stale-incarnation "expected the session line for $member and no rule 1 line for $leader, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null

# Green: a session whose members all started before the window is not this run's
# business, even though the record pins it.
oldsession="$scratch/old-session"
mkdir -p "$oldsession/state" "$oldsession/pids"
production_session "$oldsession/pids" "$oldsession/state/runtime.json"
leader=$(cat "$oldsession/pids/leader")
out=$("$check" --root "$oldsession" --window-start "$(( $(ticks_now) + 100000 ))" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report session-before-window ok
else
  report session-before-window "expected leak=0 rc=0, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null

# Green: dead member, grandchild older than the window. The session branch must
# filter on the member's own start time, not on the record's.
oldorphan="$scratch/old-orphan"
mkdir -p "$oldorphan/state" "$oldorphan/pids"
production_session "$oldorphan/pids" "$oldorphan/state/runtime.json"
leader=$(cat "$oldorphan/pids/leader")
member=$(cat "$oldorphan/pids/member")
kill -9 "$member" 2>/dev/null
wait "$member" 2>/dev/null
kill -9 "$leader" 2>/dev/null
wait "$leader" 2>/dev/null

sleep 0.1
out=$("$check" --root "$oldorphan" --window-start "$(( $(ticks_now) + 100000 ))" 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && [ "$out" = "leak=0" ]; then
  report orphan-before-window ok
else
  report orphan-before-window "expected leak=0 rc=0, got rc=$rc out=$out"
fi
kill -- "-$leader" 2>/dev/null


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
