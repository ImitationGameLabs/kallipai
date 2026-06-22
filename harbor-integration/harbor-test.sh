#!/usr/bin/env bash
# Run just-agent through Harbor's benchmarking pipeline.
# Defaults to hello-world for quick verification.
#
# Build method is auto-detected:
#   - If `nix` is on PATH → nix build (recommended; uses crane caching)
#   - Otherwise            → cargo build --release
#
# Usage:
#   ./harbor-integration/harbor-test.sh                              # hello-world
#   ./harbor-integration/harbor-test.sh terminal-bench-2             # full eval
#   ./harbor-integration/harbor-test.sh --config path/to/custom.yaml # custom
#
# Caveat (cargo fallback):
#   Without nix, binaries are compiled and linked by cargo directly against
#   the system toolchain — no crane incremental build caching.
#
# Prerequisites:
#   - Completed setup (./harbor-integration/setup-venv.sh)
#   - Provider API key exported (e.g. JUST_LLM_DEEPSEEK_API_KEY)
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# -- Auto-detect build method ------------------------------------------------

if command -v nix >/dev/null 2>&1; then
  echo "==> Detected nix — using nix build"
  _use_cargo=""
else
  echo "==> nix not found — falling back to cargo build"
  _use_cargo=1
fi

# -- Select config -----------------------------------------------------------

config="harbor-integration/configs/hello-world.yaml"

if [[ $# -gt 0 && "$1" != --* ]]; then
  config="harbor-integration/configs/$1.yaml"
  shift
fi

if [[ ! -f "$config" ]]; then
  echo "error: config not found: $config" >&2
  echo "available configs:" >&2
  ls harbor-integration/configs/*.yaml | xargs -n1 basename | sed 's/\.yaml$//' >&2
  exit 1
fi

# -- Ensure venv exists ------------------------------------------------------

if [[ ! -d harbor-integration/.venv ]]; then
  echo "==> Running setup first..."
  bash harbor-integration/setup-venv.sh
  echo ""
fi

# -- Build tarball and point venv at it --------------------------------------

if [[ "$_use_cargo" == "1" ]]; then
  echo "==> Building with cargo (release)..."
  cargo build --release

  version="$(git describe --tags --always --dirty 2>/dev/null || echo dev)"
  tarball_dir="target/just-agent-tarball"
  rm -rf "$tarball_dir"
  mkdir -p "$tarball_dir/bin"

  binaries=(just-agent just-agent-daemon just-agent-run just-agent-tui)
  for bin in "${binaries[@]}"; do
    if [[ ! -f "target/release/$bin" ]]; then
      echo "error: expected binary not found: target/release/$bin" >&2
      exit 1
    fi
    cp "target/release/$bin" "$tarball_dir/bin/"
  done

  tar -czf "$tarball_dir/just-agent-${version}-linux-x86_64.tar.gz" -C "$tarball_dir" bin/
  pkg_path="$tarball_dir/just-agent-${version}-linux-x86_64.tar.gz"
else
  echo "==> Building tarballs (nix)..."
  nix build .#just-agent-tarball --out-link result-just-agent
  nix build .#aifed-tarball      --out-link result-aifed

  pkg_path="$(readlink -f result-just-agent/*.tar.gz)"
  aifed_pkg_path="$(readlink -f result-aifed/*.tar.gz)"
fi

activate="harbor-integration/.venv/bin/activate"
sed -i '/^export JUST_AGENT_PACKAGE_PATH=/d; /^export AIFED_PACKAGE_PATH=/d' "$activate" 2>/dev/null || true
printf 'export JUST_AGENT_PACKAGE_PATH="%s"\n' "$pkg_path" >> "$activate"
# aifed-tarball is only built in the nix path; leave AIFED_PACKAGE_PATH unset
# in cargo mode and the adapter skips it (optional package).
if [[ -n "${aifed_pkg_path:-}" ]]; then
  printf 'export AIFED_PACKAGE_PATH="%s"\n' "$aifed_pkg_path" >> "$activate"
fi

# -- Activate and run --------------------------------------------------------

source "harbor-integration/.venv/bin/activate"

echo "==> harbor run --config $config $*"
exec harbor run --config "$config" "$@"
