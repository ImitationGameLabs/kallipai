# Pinned 1.97 toolchain for the FHS distribution instance, assembled
# from the official dist tarballs.
#
# Neither oxalica rust-overlay nor fenix can supply a toolchain here:
# both carry lib internals that need `makeScopeWithSplicing'`, absent
# from nixos-22.11's lib, so their callPackage context cannot resolve
# them (fenix evaluates fine as a standalone flake but breaks once
# crane's spliceToolchain drives it through the 22.11 lib). The official
# dist is a static payload — its binaries run on any glibc >= 2.17 — so
# this wrapper needs only plain stdenv and keeps the 22.11 instance
# self-contained. Rustc and cargo link against the *build host's*
# glibc (2.35 here), which is what caps the artifact floor.
{
  pkgs,
}:
let
  dist = "https://static.rust-lang.org/dist";
in
pkgs.stdenv.mkDerivation {
  name = "rust-toolchain-1.97.0";

  # The official dist binaries carry an FHS interpreter
  # (/lib64/ld-linux-...), which does not exist inside the nix build
  # sandbox. autoPatchelfHook rewrites interpreter + rpath to the
  # host stdenv's glibc (22.11 = 2.35 when built on the Fhs instance),
  # keeping the binaries runnable in-sandbox and the glibc floor at
  # the pinned 2.35.
  nativeBuildInputs = [
    pkgs.autoPatchelfHook
    pkgs.patchelf
  ];
  # libLLVM/rust-lld need libz; from the same pinned instance so the
  # loader/rpath story stays single-generation.
  buildInputs = [ pkgs.zlib ];

  rustDist = pkgs.fetchurl {
    url = "${dist}/rust-1.97.0-x86_64-unknown-linux-gnu.tar.xz";
    sha256 = "1cf17e4905b841d4c8e3f76467ac148d55fb3f54bf213c86f0d287a36471d904";
  };
  srcDist = pkgs.fetchurl {
    url = "${dist}/rust-src-1.97.0.tar.xz";
    sha256 = "f2530acd6b4da25a21151e3560d908b31f082edd6968f96d2682c8c2c15c66b7";
  };

  # The dist ships pre-built, pre-stripped binaries. autoPatchelfHook
  # (fixupPhase) handles interpreter + shared-library rpath; stripping
  # upstream binaries is pointless, so skip it.
  dontStrip = true;
  dontUnpack = true;

  installPhase = ''
    runHook preInstall

    mkdir -p "$out" rust rustsrc
    tar xf "$rustDist" -C rust --strip-components=1
    tar xf "$srcDist" -C rustsrc --strip-components=1
    bash rust/install.sh --prefix="$out" --without=rust-docs
    bash rustsrc/install.sh --prefix="$out"

    runHook postInstall
  '';
}
