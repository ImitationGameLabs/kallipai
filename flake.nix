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

    # aifed: process-level dependency (shell-out, not a Cargo dep). The
    # NixOS module installs it from here via overlays.default, so
    # pkgs.aifed is the single source of truth (identical store path to
    # `nix build .#aifed`). packages re-exports aifed-tarball where aifed
    # provides it.
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

      # Flake-level output: the NixOS module exposing the services as
      # system services. It is flake-level, not perSystem: NixOS modules
      # are system-agnostic. 'default' follows the flake convention. The
      # module receives the whole packages set and resolves its defaults
      # per host system
      # (packages.${pkgs.stdenv.hostPlatform.system}); the reference
      # stays lazy and the export system-agnostic.
      flake = {
        nixosModules.kallipai = import ./nix/nixos-modules.nix {
          inherit (self) packages;
          aifedOverlay = inputs.aifed.overlays.default;
        };
        nixosModules.default = self.nixosModules.kallipai;
      };

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

          # Shared derivations, defined once at the per-system level so
          # checks and packages reference the bit-identical workspace.
          sharedSkills = import ./nix/packages/shared-skills.nix {
            inherit pkgs;
          };
          builds = import ./nix/packages/workspace.nix {
            inherit common pkgs sharedSkills;
          };
          inherit (builds) workspace;

          checks = import ./nix/checks.nix {
            inherit pkgs common;
            inherit (inputs) advisory-db;
            inherit (inputs) aifed;
            inherit (inputs.nixpkgs) lib;
          };

          # Shared devShell concerns (repo-wide tooling + opt-in sccache)
          # consumed by both devShells.default and devShells.tauri — see
          # nix/devshells/shared.nix.
          shared = import ./nix/devshells/shared.nix {
            inherit pkgs lib;
          };

          packages = {
            inherit workspace;
            default = workspace;
            # Per-crate binaries (archeion; lesche; files; tagma). Cross-platform:
            # plain Rust builds. Their docker images are Linux-only (see
            # kallip-archeion-image / kallip-lesche-image / kallip-files-image /
            # kallip-tagma-image below).
            kallip-archeion = builds.archeion;
            kallip-admin = builds.admin;
            kallip-lesche = builds.lesche;
            kallip-files = builds.files;
            kallip-tagma = builds.tagma;
            kallip-cron-daemon = builds.cron-daemon;
            kallip-cron = builds.cron;
            kallip-daemon = builds.daemon;
            kallipctl = builds.ctl;
            kallip-daemon-spawn = builds.daemon-spawn;
            kallip-instances = builds.instances;
            kallip-tarball = import ./nix/packages/tarball.nix {
              inherit
                pkgs
                common
                workspace
                ;
            };
            # Re-export the per-system-level sharedSkills derivation as a
            # package so deployments can reference the bit-identical store
            # path directly.
            kallip-shared-skills = sharedSkills;
          }
          # Container images: scratch + nix closure via dockerTools. Linux-only
          # (the buildImage closure is Linux-native). See
          # nix/packages/docker-images/.
          // (lib.optionalAttrs pkgs.stdenv.isLinux {
            # Purpose-built prod images for the split deploy
            # (compose/prod/polis.nix / tagma.nix): archeion, lesche, and
            # files are the server-side services (co-located, independent images);
            # carries no tagma-specific baked env.
            kallip-archeion-image = import ./nix/packages/docker-images/archeion.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) archeion admin;
            };
            kallip-lesche-image = import ./nix/packages/docker-images/lesche.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) lesche;
            };
            kallip-files-image = import ./nix/packages/docker-images/files.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) files;
            };
            kallip-tagma-image = import ./nix/packages/docker-images/tagma.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) tagma;
            };
            # Pre-built integration-test binaries + the agent binaries, for
            # running the suite in a container (see compose/dev/test.nix).
            # Linux-only like the image.
            kallip-integration-tests = import ./nix/packages/integration-tests.nix {
              inherit
                pkgs
                common
                workspace
                ;
            };
            # The kallip-web static site (SPA bundle for a static file
            # server to serve; the NixOS module derives the site root,
            # services.kallipai.web.distWithRuntimeConfig, from it).
            # Linux-only:
            # the node_modules
            # dependency tree carries platform binaries.
            kallip-web-dist = import ./nix/packages/kallip-web.nix {
              inherit pkgs;
              src = self;
              inherit (pkgs) deno;
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
              shared
              ;
          };

          # Backend toolchain only (Rust + TS + Nix) — see nix/devshells/default.nix.
          defaultDevShell = import ./nix/devshells/default.nix {
            inherit
              pkgs
              lib
              common
              shared
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
