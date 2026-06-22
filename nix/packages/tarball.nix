{
  pkgs,
  lib,
  common,
}:
let
  inherit (common)
    craneLib
    commonArgs
    cargoArtifacts
    gitVersion
    ;

  # Build the entire workspace at once, then pick the binaries we need.
  # Packaging only: doCheck = false HERE so `nix build` doesn't run `cargo test`
  # (whose sandbox-env deps — CA roots, pgrep/kill — would pollute the package).
  # NB: do NOT hoist this into commonArgs. crane's cargoNextest and buildDepsOnly
  # both do `args.doCheck or true`, so a shared doCheck = false would silently skip
  # nextest's checkPhase AND drop buildDepsOnly's dev-dep caching. Keep it here.
  workspace = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;
    }
  );

  # Standard FHS interpreter path used by most Linux distributions.
  fhs-interp = "/lib64/ld-linux-x86-64.so.2";
in
{
  # Tarball containing all workspace binaries, suitable for installation
  # inside containers (e.g. Harbor benchmarking containers).
  #
  # Nix-built binaries hardcode /nix/store/…/ld-linux as their ELF
  # interpreter, which does not exist in standard container images.
  # We use patchelf to rewrite the interpreter and rpath so the
  # binaries run on any FHS-compliant Linux (Ubuntu, Debian, etc.).
  just-agent-tarball =
    pkgs.runCommand "just-agent-tarball"
      {
        nativeBuildInputs = with pkgs; [
          gnutar
          patchelf
        ];
      }
      ''
        mkdir -p $out
        cp -r ${workspace}/bin bin
        chmod -R u+w bin

        # Patch ELF interpreter and remove Nix-specific rpath.
        for bin in bin/*; do
          patchelf --set-interpreter ${fhs-interp} "$bin"
          patchelf --remove-rpath "$bin"
        done

        tar -czf $out/just-agent-${gitVersion}-linux-x86_64.tar.gz bin/
      '';
}
