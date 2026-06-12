{
  description = "just-agent — agentic AI agent runtime built in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    inputs@{ self, flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { system, lib, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };

          root = ./.;

          common = import ./nix/common.nix {
            inherit
              pkgs
              lib
              inputs
              root
              ;
          };

          checks = import ./nix/checks.nix {
            inherit pkgs common;
            inherit (inputs) advisory-db;
          };

          packages = import ./nix/packages/tarball.nix {
            inherit pkgs lib common;
          };

          inherit (common) craneLib;
        in
        {
          inherit checks packages;

          devShells.default = craneLib.devShell {
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

              # Markdown formatter
              nodePackages.prettier

              # Temporary workaround for copilot-cli direnv integration bug
              # See: https://github.com/github/copilot-cli/issues/731
              # TODO: Remove once the upstream issue is resolved
              bashInteractive
            ];
          };
        };
    };
}
