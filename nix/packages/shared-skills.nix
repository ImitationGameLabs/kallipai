{ pkgs }:
# The curated shared-skill tree (repo-root `skills/`) packaged as an immutable
# FHS-layout derivation. Consumed two ways:
#   - flake package `kallip-shared-skills` (pinned via flake.lock for any
#     external consumer);
#   - `container-shared.nix`, which imports this same file so the tagma docker
#     image and the arion dev compose reference a bit-identical store path.
#
# The tagma seeds `<data_dir>/skills/` from this tree on first boot (see
# `seed_skills_if_empty` in crates/kallip-runtime); it never serves skills
# directly from this read-only path.
#
# `${../../skills}` is a path literal relative to this file's location
# (`nix/packages/` -> repo root). As a flake-path it is VCS-filtered, so only
# git-tracked skill files enter the store.
pkgs.runCommand "kallip-shared-skills" { } ''
  mkdir -p $out/share/kallip/skills
  cp -r ${../../skills}/. $out/share/kallip/skills/
''
