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

            config = {
              allowUnfree = true;
              android_sdk.accept_license = true;
            };
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
              # Crane binary builds (full workspace + per-crate subsets for the
              # purpose-built images), all on the shared deps cache. See
              # nix/packages/workspace.nix.
              builds = import ./nix/packages/workspace.nix {
                inherit common;
              };
              # The full-workspace build, passed to the tarball + integration
              # tests (they take a single `workspace` derivation).
              workspace = builds.workspace;
            in
            {
              default = workspace;
              # Per-crate binaries (agora; lesche; tagma+herald). Cross-platform:
              # plain Rust builds. Their docker images are Linux-only (see
              # kallip-agora-image / kallip-lesche-image / kallip-tagma-image below).
              kallip-agora = builds.agora;
              kallip-lesche = builds.lesche;
              kallip-tagma = builds.tagma;
              kallip-tarball = import ./nix/packages/tarball.nix {
                inherit
                  pkgs
                  common
                  workspace
                  ;
              };
            }
            # Container images: scratch + nix closure via dockerTools. Linux-only
            # (the buildImage closure is Linux-native). See
            # nix/packages/docker-images/.
            // (lib.optionalAttrs pkgs.stdenv.isLinux {
              # Purpose-built prod images for the split deploy
              # (nix/prod-composes/agora.nix / tagma.nix): agora + lesche are the
              # two server-side services (co-located, independent images); tagma
              # carries no tagma-specific baked env.
              kallip-agora-image = import ./nix/packages/docker-images/agora.nix {
                inherit
                  pkgs
                  common
                  ;
                inherit (builds) agora;
              };
              kallip-lesche-image = import ./nix/packages/docker-images/lesche.nix {
                inherit
                  pkgs
                  common
                  ;
                inherit (builds) lesche;
              };
              kallip-tagma-image = import ./nix/packages/docker-images/tagma.nix {
                inherit
                  pkgs
                  common
                  ;
                inherit (builds) tagma;
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

          # Opt-in devShell for the kallip-app Android (Tauri mobile) target.
          # The app's Rust is a standalone Cargo project (not a root-workspace
          # member), so this is intentionally a separate shell with its own
          # Android toolchain — see nix/devshells/tauri.nix. Entered via `nix
          # develop .#tauri`.
          tauriDevShell = import ./nix/devshells/tauri.nix {
            inherit
              pkgs
              lib
              inputs
              ;
          };

          # Backend toolchain only (Rust + TS + Nix) — see nix/devshells/default.nix.
          defaultDevShell = import ./nix/devshells/default.nix {
            inherit
              pkgs
              lib
              common
              ;
          };
        in
        {
          inherit checks packages;

          devShells = {
            default = defaultDevShell;
            tauri = tauriDevShell;
          };
        };
    };
}
