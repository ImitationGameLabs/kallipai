{
  pkgs,
  lib,
  inputs,
  # Optional toolchain override: a toolchain derivation applied by
  # overrideToolchain. Null keeps the default (stable latest from
  # rust-overlay on the caller's pkgs).
  rustToolchain ? null,

  # Pinned 22.11 instance: when set, the build layer is driven by it —
  # the linker (its stdenv cc) supplies -L/dynamic-linker/crt/libgcc so
  # crt and libc stay same-source, and openssl comes from 22.11 too.
  # Crane/cargo/rustc themselves stay on the main nixpkgs; the artifact
  # glibc floor is decided by the LINKING host's libc = 2.35. Null
  # keeps the caller's pkgs for every layer (the main build).
  pkgsFhs ? null,
  # Project root path, must be passed from flake.nix (not ./.) because:
  # - Nix paths are evaluated at definition site
  # - If we use ./. here, it would resolve to nix/ directory, not project root
  # - Passing from flake.nix ensures ./. resolves to the correct location
  root,
}:
let
  # NB: we don't need to overlay our custom toolchain for the *entire*
  # pkgs (which would require rebuiding anything else which uses rust).
  # Instead, we just want to update the scope that crane will use by appending
  # our specific toolchain there.
  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain (
    if rustToolchain != null then
      p: rustToolchain
    else
      p:
      p.rust-bin.stable.latest.default.override {
        extensions = [ "rust-src" ];
        # targets = [ "wasm32-wasip1" ];
      }
  );

  # Allowlist branch over the crane cargo filter: any route-shapes.json
  # under any crate joins the build source. Written as a single
  # cleanSourceWith (an outer layer over cleanCargoSource cannot revive
  # files, because cleanSourceWith composes filters with AND); this
  # expands cleanCargoSource and adds one OR branch.
  src = lib.cleanSourceWith {
    src = lib.cleanSource root;
    filter =
      path: type: craneLib.filterCargoSources path type || baseNameOf path == "route-shapes.json";
    name = "kallipai-source";
  };

  # Use git shortRev as version, fallback to "dirty" if working tree is dirty
  gitVersion = inputs.self.shortRev or "dirty";

  # Common build arguments shared across all crate builds.
  # Includes source path, dependencies, and platform-specific inputs.
  commonArgs = {
    inherit src;
    strictDeps = true;

    nativeBuildInputs =
      with pkgs;
      [
        pkg-config
      ]
      ++ lib.optionals (pkgsFhs != null) [
        # The 22.11 linker: drives the whole link layer (-L, dynamic
        # linker, crt objects, libgcc) from the pinned glibc, keeping
        # crt/libc same-source.
        pkgsFhs.stdenv.cc
      ];

    buildInputs =
      (
        # Kept even though the binaries end up not linking libssl
        # (reqwest rides rustls): openssl-sys still needs it to
        # compile when the vendored openssl feature is enabled.
        if pkgsFhs != null then
          with pkgsFhs;
          [
            openssl
          ]
        else
          with pkgs;
          [
            openssl
          ]
      )
      ++ lib.optionals pkgs.stdenv.isDarwin [
        # Additional darwin specific inputs can be set here
        pkgs.libiconv
      ];
  }
  # Route cargo's linker to the 22.11 cc wrapper (see above). The path
  # is spelled out here so the wrapper derivation enters the closure via
  # nativeBuildInputs; the env var itself only carries the string.
  // lib.optionalAttrs (pkgsFhs != null) {
    env.CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgsFhs.stdenv.cc}/bin/cc";
  };

  # Build *just* the cargo dependencies (of the entire workspace),
  # so we can reuse all of that work (e.g. via cachix) when running in CI
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

in
{
  inherit
    craneLib
    src
    commonArgs
    cargoArtifacts
    gitVersion
    ;
}
