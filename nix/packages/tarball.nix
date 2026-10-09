{
  pkgs,
  common,
  # The crane-built workspace derivation (packages.default). Shared so the
  # tarball never rebuilds or duplicates it.
  workspace,
  # The installer script and the unit templates ship beside the binaries,
  # so the tarball alone is a complete install form (no repo, no nix).
  installScript,
  systemdTemplates,
}:
let
  inherit (common) gitVersion;

  # Standard FHS interpreter path used by most Linux distributions.
  fhs-interp = "/lib64/ld-linux-x86-64.so.2";
in
# Self-contained FHS install form: the binaries, install.sh, and the
# systemd unit templates travel together, so an extraction plus sudo
# ./install.sh --with-systemd installs fully without a repository or
# nix (also consumed directly inside Harbor benchmarking containers).
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
    # Self-sufficiency inputs: install.sh resolves the templates from
    # the unpacked payload's systemd/ directory at install time.
    install -m 0755 ${installScript} install.sh
    mkdir systemd
    cp ${systemdTemplates}/*.service.in systemd/

    tar -czf $out/kallipai-${gitVersion}-linux-x86_64.tar.gz bin/ install.sh systemd/
  ''
