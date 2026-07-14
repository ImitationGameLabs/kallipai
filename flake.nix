{
  description = "kallip — agentic AI agent runtime built in Rust";

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

    # aifed: intended process-level dep (shell-out, not a Cargo dep); runtime
    # adoption pending. Consumed via overlays.default, so pkgs.aifed is the
    # single source of truth (identical store path to `nix build .#aifed`).
    # packages re-exports aifed-tarball where aifed provides it.
    aifed = {
      url = "github:ImitationGameLabs/aifed";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        rust-overlay.follows = "rust-overlay";
        crane.follows = "crane";
      };
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
            overlays = [
              (import inputs.rust-overlay)
              inputs.aifed.overlays.default
            ];
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

          packages =
            let
              # The crane-built workspace: every kallip binary in one
              # derivation. This is packages.default and the single source of
              # truth consumed by the tarball and docker image packages.
              workspace = import ./nix/packages/workspace.nix {
                inherit lib common;
              };
            in
            {
              default = workspace;
              kallip-tarball = import ./nix/packages/tarball.nix {
                inherit
                  pkgs
                  common
                  workspace
                  ;
              };
            }
            # Container image: scratch + nix closure via dockerTools. Linux-only
            # (the buildImage closure is Linux-native). Shares the same
            # workspace derivation as packages.default.
            // (lib.optionalAttrs pkgs.stdenv.isLinux {
              kallip-image = import ./nix/packages/container-image.nix {
                inherit
                  pkgs
                  common
                  workspace
                  ;
              };
              # Pre-built integration-test binaries + the agent binaries, for
              # running the suite in a container (see arion-compose.nix test
              # mode). Linux-only like the image.
              kallip-integration-tests = import ./nix/packages/integration-tests.nix {
                inherit
                  pkgs
                  common
                  workspace
                  ;
              };
            })
            # Re-export aifed's FHS tarball so the benchmark pins both in one lock.
            # Gate on aifed's actual availability: the pinned rev ships aifed-tarball
            # on x86_64-linux only (aarch64-linux once aifed adds it) — auto-adapts.
            // (lib.optionalAttrs (inputs.aifed.packages.${system} ? aifed-tarball) {
              inherit (inputs.aifed.packages.${system}) aifed-tarball;
            });

          inherit (common) craneLib;
        in
        {
          inherit checks packages;

          devShells.default = craneLib.devShell {
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
          };
        };
    };
}
