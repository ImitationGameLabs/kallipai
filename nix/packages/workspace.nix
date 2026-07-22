{
  common,
}:
let
  inherit (common)
    craneLib
    commonArgs
    cargoArtifacts
    ;

  # One crane build recipe for a chosen cargo command, on the shared workspace
  # deps cache. `workspace` builds every binary in one derivation; `agora` /
  # `tagma` build per-crate subsets so the purpose-built docker images carry
  # only the binaries they need (lighter closures -- see docker-images/).
  #
  # NB: doCheck = false so `nix build .#default` doesn't run `cargo test` (whose
  # sandbox-env deps -- CA roots, pgrep/kill -- would pollute the package).
  # NB: do NOT hoist doCheck into commonArgs. crane's cargoNextest and
  # buildDepsOnly both do `args.doCheck or true`, so a shared doCheck = false
  # would silently skip nextest's checkPhase AND drop buildDepsOnly's dev-dep
  # caching. Keep it on this buildPackage call only.
  buildCrate =
    cargoBuildCommand:
    craneLib.buildPackage (
      commonArgs
      // {
        inherit cargoArtifacts cargoBuildCommand;
        doCheck = false;
      }
    );
in
{
  # The full workspace: every kallip binary. This is `packages.default` and the
  # single source of truth consumed by the tarball + dev compose.
  workspace = buildCrate "cargo build --release";
  # The agora control-plane server (pure HTTP/Postgres; no shell-out deps).
  agora = buildCrate "cargo build --release -p kallip-agora";
  # The lesche data-plane relay (herald tunnels, app SSE, envelope routing; pure
  # HTTP, no shell-out deps). Its own image so the agora and lesche services
  # deploy independently -- see nix/packages/docker-images/lesche.nix.
  lesche = buildCrate "cargo build --release -p kallip-lesche";
  # The host/"tagma" side: the tagma service (agent server) + herald share most
  # of their closure, so one build beats two. Excludes agora.
  tagma = buildCrate "cargo build --release -p kallip-tagma -p kallip-herald";
}
