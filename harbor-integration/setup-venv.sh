#!/usr/bin/env bash
# Create and configure the Harbor integration venv.
#
# Usage:
#   ./harbor-integration/setup-venv.sh
#
# After running, activate the environment:
#   source harbor-integration/.venv/bin/activate
#
# Prerequisites:
#   - Nix with flake support (provides libstdc++ and other system libs)
#   - uv (https://docs.astral.sh/uv/)
#   - podman-docker (for podman compatibility with Harbor)
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

echo "==> Creating venv (uv, with system-site-packages for NixOS compat)..."
if [ ! -d harbor-integration/.venv ]; then
  uv venv harbor-integration/.venv --system-site-packages
fi

echo "==> Installing adapter + harbor..."
uv pip install -e ./harbor-integration --python harbor-integration/.venv/bin/python
uv pip install harbor --python harbor-integration/.venv/bin/python

echo ""
echo "Done. Next steps:"
echo ""
echo "  1. cp harbor-integration/localprofile.example.toml harbor-integration/.localprofile.toml"
echo "     and fill in your provider key and model (see harbor-integration/README.md)"
echo "  2. ./harbor-integration/harbor-test.sh"
