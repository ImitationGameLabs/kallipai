#!/usr/bin/env bash
# build-and-load-images.sh -- build the KallipAI container images and load them
# into the local docker daemon.
#
# The one-shot form of the plain-docker flow in
# docs/en/deployment/container.md ("Without Arion"): every image is built with
# `nix build --no-link --print-out-paths` and the printed store path is handed
# to `docker load -i`. No `result` symlink is created, overwritten, or read, so
# the script cannot clobber whatever `result` in the checkout points at.
#
# The images are Linux-native (nixpkgs dockerTools); build them on Linux.
#
# Usage: scripts/build-and-load-images.sh [image ...]
#
#   image       Build only the named images instead of all five.
#               names: archeion lesche keryx files tagma
#   -h, --help  Show this help.
#
# Exit status: 0 when every selected image built and loaded. The run stops at
# the first failure, leaving the remaining images untouched.
set -euo pipefail

# The authoritative image list: short name -> flake attribute, one entry per
# line, in deploy order (the platform services first, the tagma host last).
# The short name is the selector on the command line; the attribute is what
# nix builds.
IMAGES=(
  "archeion:kallipai-archeion-image"
  "lesche:kallipai-lesche-image"
  "keryx:kallipai-keryx-image"
  "files:kallipai-files-image"
  "tagma:kallipai-tagma-image"
)

die() { echo "build-and-load-images.sh: error: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: scripts/build-and-load-images.sh [image ...]

Build the KallipAI container images with nix and load each one into the local
docker daemon. With no arguments every image is built and loaded.

Images (short name -> flake attribute):
  archeion  kallipai-archeion-image
  lesche    kallipai-lesche-image
  keryx     kallipai-keryx-image
  files     kallipai-files-image
  tagma     kallipai-tagma-image

Arguments:
  image       Build only the named images instead of all five.
  -h, --help  Show this help.
EOF
}

# attr_for NAME -- print the flake attribute behind a short name; non-zero when
# the name is not in the list.
attr_for() {
  local want="$1" entry
  for entry in "${IMAGES[@]}"; do
    if [ "${entry%%:*}" = "$want" ]; then
      printf '%s\n' "${entry#*:}"
      return 0
    fi
  done
  return 1
}

selected=()
while [ $# -gt 0 ]; do
  case "$1" in
  -h | --help)
    usage
    exit 0
    ;;
  -*)
    usage >&2
    die "unknown argument: $1"
    ;;
  *)
    selected+=("$1")
    shift
    ;;
  esac
done

# Default: every image, in list order.
if [ ${#selected[@]} -eq 0 ]; then
  for entry in "${IMAGES[@]}"; do
    selected+=("${entry%%:*}")
  done
fi

# Reject an unknown selector before doing any work.
for name in "${selected[@]}"; do
  attr_for "$name" >/dev/null || {
    usage >&2
    die "unknown image: $name"
  }
done

# Fail before the expensive part: a missing tool or a daemon that is not
# running would otherwise surface only after the images are built.
command -v nix >/dev/null || die "nix not found on PATH"
command -v docker >/dev/null || die "docker not found on PATH"
docker info >/dev/null 2>&1 || die "docker daemon not reachable"

# Build from the repository root -- the flake's own directory -- whatever the
# caller's working directory is.
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repo_root"

total=${#selected[@]}
index=0
for name in "${selected[@]}"; do
  index=$((index + 1))
  attr="$(attr_for "$name")"
  printf '[%d/%d] %s: building %s\n' "$index" "$total" "$name" "$attr"
  path="$(nix build --no-link --print-out-paths ".#$attr")"
  [ -n "$path" ] || die "$name: nix build printed no store path"
  [ -f "$path" ] || die "$name: no image tarball at $path"
  printf '[%d/%d] %s: loading %s\n' "$index" "$total" "$name" "$path"
  docker load -i "$path"
done
printf 'done: %d image(s) loaded; docker images | grep kallipai\n' "$total"
