# Default devShell: backend toolchain only (Rust + TS + Nix).
# Entered via `nix develop` (or direnv with KALLIP_DEVSHELL unset/default).
#
# sccache (shared rustc cache across worktrees) is opt-in via USE_SCCACHE=1;
# the mechanism lives in nix/devshells/shared.nix (shared.withSccache).
{
  pkgs,
  lib,
  common,
  shared,
}:

common.craneLib.devShell (
  shared.withSccache {
    # Extra inputs can be added here; cargo and rustc are provided by default.
    packages =
      shared.tooling
      # Root-workspace only (the app's Cargo project is not a workspace member).
      ++ [ pkgs.cargo-hakari ]
      # aifed is Linux-only; keep it out of the darwin devShell.
      ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.aifed ];
  }
)
