{
  pkgs,
  common,
  checks,
}:
let
  inherit (common) craneLib;
in
craneLib.devShell {
  inherit checks;

  # Extra inputs can be added here; cargo and rustc are provided by default.
  packages = with pkgs; [
    # Rust
    cargo-hakari
    rust-analyzer

    # Nix
    nil
    nixfmt
    statix

    # TOML toolkit (linter, formatter)
    taplo

    # Sandboxing tools
    bubblewrap

    # Temporary workaround for copilot-cli direnv integration bug
    # See: https://github.com/github/copilot-cli/issues/731
    # TODO: Remove once the upstream issue is resolved
    bashInteractive
  ];
}
