{
  pkgs,
  common,
  # The crane-built workspace derivation (packages.default). Shared so the
  # tarball never rebuilds or duplicates it.
  workspace,
}:
let
  inherit (common) gitVersion;

  # Standard FHS interpreter path used by most Linux distributions.
  fhs-interp = "/lib64/ld-linux-x86-64.so.2";
in
# Tarball containing all workspace binaries, suitable for installation
# inside containers (e.g. Harbor benchmarking containers).
#
# Nix-built binaries hardcode /nix/store/…/ld-linux as their ELF
# interpreter, which does not exist in standard container images.
# We use patchelf to rewrite the interpreter and rpath so the
# binaries run on any FHS-compliant Linux (Ubuntu, Debian, etc.).
pkgs.runCommand "kallipai-tarball"
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
    # The workspace's kallipai-tagma is the skills-seed wrapper script; its
    # store-path shebang and seed value do not exist on FHS hosts. Ship the
    # wrapped ELF in its place -- tarball targets carry no seed default.
    if [ -e bin/.kallipai-tagma-wrapped ]; then
      mv bin/.kallipai-tagma-wrapped bin/kallipai-tagma
    fi

    # Patch ELF interpreter and remove Nix-specific rpath. Skip non-ELF
    # files: the kallipai-tagma skills-seed wrapper is a shell script, and
    # patchelf errors out on anything but ELF headers.
    for bin in bin/*; do
      if [ "$(head -c 4 "$bin")" != $'\x7fELF' ]; then
        continue
      fi
      patchelf --set-interpreter ${fhs-interp} "$bin"
      patchelf --remove-rpath "$bin"
    done

    tar -czf $out/kallipai-${gitVersion}-linux-x86_64.tar.gz bin/
  ''
