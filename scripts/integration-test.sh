#!/usr/bin/env bash
# integration-test.sh -- build the integration-test image, run the suites in a
# container, and exit with their verdict.
#
#   scripts/integration-test.sh
#
# Exit status: the container's own exit code, 124 when the run outlives
# INTEGRATION_TIMEOUT (default 45m), and 1 when the run cannot prove the suites
# finished (see "Verdict" below).
#
# Verdict: the exit code is the whole verdict, and it comes from
# `docker compose up --exit-code-from`. A 0 alone does not mean the suites
# passed -- `docker compose ps -a` exits 0 whatever the container did, and a
# container that never ran anything still exits 0. So a green run must also
# show one `=== integration test: <path> ===` line per shipped suite and the
# runner's closing `=== integration tests: all suites passed ===` line. Log
# wording never overrides the exit code: libtest's `test result:` lines are
# printed to say what failed, not to decide it.
set -euo pipefail

cd "$(dirname "$0")/.."

# Overridable so a probe can run a deliberately broken command against the
# same image; the default is the only file a normal run touches.
readonly COMPOSE_FILE="${INTEGRATION_COMPOSE_FILE:-compose/compose.test.yaml}"
readonly LOG="${INTEGRATION_LOG:-/tmp/integration-tests.log}"
readonly LIMIT="${INTEGRATION_TIMEOUT:-45m}"

# The image carries symlinks into the store; the suite line count comes from
# the same derivation the image was built from, so the two cannot disagree
# about what "every suite" means.
image_path="$(nix build --no-link --print-out-paths .#kallipai-integration-tests-image)"
suites_path="$(nix build --no-link --print-out-paths .#kallipai-integration-tests)"
expected="$(find "$suites_path/integration-tests" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')"

# streamLayeredImage builds a program that streams the image to stdout, not a
# tarball on disk, so it is piped into `docker load`.
if ! load_out="$("$image_path" | docker load 2>&1)"; then
  printf '%s\n' "$load_out" >&2
  echo "docker load failed; the image stream did not produce a loadable image" >&2
  exit 1
fi
printf '%s\n' "$load_out"
image_ref="$(printf '%s\n' "$load_out" | sed -n 's/^Loaded image: //p' | tail -n 1)"
if [ -z "$image_ref" ]; then
  echo "docker load printed no 'Loaded image:' line; cannot name the image" >&2
  exit 1
fi
export KALLIPAI_TEST_IMAGE="$image_ref"

# The pipeline's first element decides; PIPESTATUS[0] must be read on the
# line right below it -- any command in between overwrites it.
# --ansi never and --no-log-prefix keep compose's own decoration out of
# the log: with the default, every line carries `integration-tests-1  | `
# and a trailing escape, and the counts below (anchored on ^ and $) read
# zero for a run that passed. Both flags need compose v2.20 or newer (measured
# on 5.5.1); an older client ignores them and every run lands in
# "compose failed before the suites ran".
set +e
timeout "$LIMIT" docker compose --ansi never -f "$COMPOSE_FILE" up \
  --no-log-prefix --abort-on-container-exit --exit-code-from integration-tests 2>&1 | tee "$LOG"
rc="${PIPESTATUS[0]}"
set -e

# expected counts the derivation's directory, suite_lines counts the run
# log: two sources on purpose, comparing a run against itself proves
# nothing.
suite_lines="$(grep -c '^=== integration test: ' "$LOG" || true)"
sentinel=0
if grep -q '^=== integration tests: all suites passed ===$' "$LOG"; then
  sentinel=1
fi

if [ "$rc" -eq 124 ]; then
  echo "integration tests timed out after $LIMIT" >&2
  # The timeout kills the compose client, not the container it started:
  # stop it here, or the next run's `ps` cannot tell which container is
  # whose. The log is taken first -- once the project is down there is
  # nothing left to read.
  docker compose --ansi never -f "$COMPOSE_FILE" logs --no-log-prefix >&2 || true
  docker compose --ansi never -f "$COMPOSE_FILE" down --remove-orphans >&2 || true
elif [ "$rc" -ne 0 ]; then
  if grep -q 'no integration tests found' "$LOG"; then
    echo "the image carries no integration-test binaries" >&2
  elif grep -q 'the image does not carry the shipped suite set' "$LOG"; then
    echo "the image's suite set differs from the shipped allowlist" >&2
  elif grep -q "exited with code $rc" "$LOG"; then
    echo "an integration suite failed (container exit $rc)" >&2
  else
    echo "compose failed before the suites ran (exit $rc)" >&2
  fi
elif [ "$sentinel" -ne 1 ] || [ "$suite_lines" -ne "$expected" ]; then
  echo "the suites did not run to completion: expected $expected suite line(s), saw $suite_lines, closing line seen=$sentinel" >&2
  rc=1
fi

if [ "$rc" -ne 0 ]; then
  docker compose --ansi never -f "$COMPOSE_FILE" logs --no-log-prefix >&2 || true
  echo "full output: $LOG" >&2
fi
exit "$rc"
