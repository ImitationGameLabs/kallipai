#!/usr/bin/env bash
# build-and-load-images.sh -- build the KallipAI container images and load them
# into the local docker daemon.
#
# The one-shot form of the plain-docker flow in
# docs/en/deployment/container.md ("Plain Docker"): every image is built with
# `nix build --no-link --print-out-paths` and the printed store path is handed
# to `docker load -i`. No `result` symlink is created, overwritten, or read, so
# the script cannot clobber whatever `result` in the checkout points at.
#
# The images are Linux-native (nixpkgs dockerTools); build them on Linux.
#
# Usage: scripts/build-and-load-images.sh [image ...]
#
#   image       Build only the named images instead of all of them.
#               names: archeion lesche keryx files model-gateway instances
#                      otelcol tempo grafana caddy-web tagma
#   -h, --help  Show this help.
#
# Exit status: 0 when every selected image built and loaded. The run stops at
# the first failure, leaving the remaining images untouched.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
env_script="$repo_root/scripts/compose-env.sh"

# The image list lives in scripts/compose-env.sh, beside the variable names,
# so the set this script builds and the set scripts/compose-up.sh loads come
# from one place and cannot drift apart. Every image's flake attribute is
# kallipai-<short name>-image.
images_all() {
  bash "$env_script" --images all
}

die() { echo "build-and-load-images.sh: error: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: scripts/build-and-load-images.sh [image ...]

Build the KallipAI container images with nix and load each one into the local
docker daemon. With no arguments every image is built and loaded.

Images (short name -> flake attribute):
EOF
  local name
  while read -r name; do
    printf '  %-14s %s\n' "$name" "kallipai-$name-image"
  done < <(images_all)
  cat <<'EOF'

Arguments:
  image       Build only the named images instead of all of them.
  -h, --help  Show this help.
EOF
}

# attr_for NAME -- print the flake attribute behind a short name; non-zero when
# the name is not in the list.
attr_for() {
  local want="$1"
  images_all | grep -qx -- "$want" || return 1
  printf 'kallipai-%s-image\n' "$want"
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
  while read -r name; do
    selected+=("$name")
  done < <(images_all)
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
cd -- "$repo_root"

total=${#selected[@]}
index=0
# Every image of one run carries one tag: each image is its own flake
# evaluation, so a second tag inside the loop means the two were not built
# from the same tree. The check compares tags, not trees: two dirty builds of
# different trees are both named dirty, and nothing here can tell them apart.
batch_tag=
for name in "${selected[@]}"; do
  index=$((index + 1))
  attr="$(attr_for "$name")"
  printf '[%d/%d] %s: building %s\n' "$index" "$total" "$name" "$attr"
  path="$(nix build --no-link --print-out-paths ".#$attr")"
  [ -n "$path" ] || die "$name: nix build printed no store path"
  [ -f "$path" ] || die "$name: no image tarball at $path"
  printf '[%d/%d] %s: loading %s\n' "$index" "$total" "$name" "$path"
  loaded=$(docker load -i "$path" | sed -n 's/^Loaded image: //p' | tail -n 1)
  [ -n "$loaded" ] || die "$name: docker load named no image"
  tag=${loaded##*:}
  if [ -z "$batch_tag" ]; then
    batch_tag=$tag
  elif [ "$tag" != "$batch_tag" ]; then
    die "$name: loaded $tag, but this batch already loaded $batch_tag; one run builds one batch"
  fi
  # The tarball names the image without a registry prefix. A stack that runs
  # behind one resolves <prefix><that name>, so the alias is added here instead
  # of leaving a hand-run `docker tag` between a load and a start.
  if [ -n "${KALLIPAI_IMAGE_PREFIX:-}" ]; then
    docker tag "$loaded" "${KALLIPAI_IMAGE_PREFIX}${loaded}"
  fi
done
echo "loaded $total image(s)"
echo "KALLIPAI_IMAGE_TAG=$batch_tag"
