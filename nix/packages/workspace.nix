{
  lib,
  common,
}:
let
  inherit (common)
    craneLib
    commonArgs
    cargoArtifacts
    ;
in
# Build the entire workspace at once. This is `packages.default` — the
# canonical source of every kallip binary, consumed by the tarball and the
# docker image packages so they never rebuild or duplicate it.
#
# NB: doCheck = false so `nix build .#default` doesn't run `cargo test` (whose
# sandbox-env deps — CA roots, pgrep/kill — would pollute the package).
# NB: do NOT hoist doCheck into commonArgs. crane's cargoNextest and
# buildDepsOnly both do `args.doCheck or true`, so a shared doCheck = false
# would silently skip nextest's checkPhase AND drop buildDepsOnly's dev-dep
# caching. Keep it on this buildPackage call only.
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    doCheck = false;
  }
)
