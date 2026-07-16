# Default devShell: backend toolchain only (Rust + TS + Nix).
# Entered via `nix develop` (or direnv with KALLIP_DEVSHELL unset/default).
{
  pkgs,
  lib,
  common,
}:

common.craneLib.devShell {
  # Extra inputs can be added here; cargo and rustc are provided by default.
  packages =
    with pkgs;
    [
      # Rust
      cargo-hakari
      rust-analyzer

      # Typescript
      deno

      # Nix
      nil
      nixfmt
      statix

      # TOML toolkit (linter, formatter)
      taplo

      # Markdown formatter
      prettier

      # Temporary workaround for copilot-cli direnv integration bug
      # See: https://github.com/github/copilot-cli/issues/731
      # TODO: Remove once the upstream issue is resolved
      bashInteractive
    ]
    # aifed is Linux-only; keep it out of the darwin devShell.
    ++ lib.optionals pkgs.stdenv.isLinux [ aifed ];
}
