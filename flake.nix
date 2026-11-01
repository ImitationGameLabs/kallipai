{
  description = "kallipai — agentic AI agent runtime built in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # Pinned glibc build target for the FHS distribution tarball:
    # nixos-22.11 carries glibc 2.35, which caps the binary's symbol
    # floor by construction — the linker takes the build host's libc.
    # The channel is EOL; acceptable here because the input is a frozen
    # link anchor, not a runtime dependency. NB: `nix flake update`
    # with no arguments updates this input too; update it deliberately
    # via `nix flake update nixpkgs-2211` only when re-validating the
    # floor.
    nixpkgs-2211.url = "github:NixOS/nixpkgs/nixos-22.11";
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

          # Pre-built integration-test binaries, shared by the flake attribute
          # and the container image that runs them.
          integrationTests = import ./nix/packages/integration-tests.nix {
            inherit
              pkgs
              common
              workspace
              ;
          };

          # Pinned 22.11 instance for the FHS distribution tarball. It
          # supplies only the link layer: its stdenv cc is the cargo
          # linker, so crt objects, -L search order, the dynamic linker,
          # and libgcc all come from glibc 2.35 — same-source with the
          # libc that resolves the symbols. Crane, cargo, and rustc stay
          # on the main nixpkgs; the toolchain is the official-dist 1.97
          # derivation.
          pkgsFhs = import inputs.nixpkgs-2211 {
            inherit system;

            config = {
              allowUnfree = true;
            };
          };

          commonFhs = import ./nix/common.nix {
            inherit
              pkgs
              lib
              inputs
              root
              pkgsFhs
              ;
            rustToolchain = import ./nix/packages/fhs-rust-toolchain.nix {
              pkgs = pkgsFhs;
            };
          };

          buildsFhs = import ./nix/packages/workspace.nix {
            common = commonFhs;
            inherit pkgs sharedSkills;
          };

          checks = import ./nix/checks/default.nix {
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
            # Per-crate binaries (archeion; lesche; keryx; files; tagma). Cross-platform:
            # plain Rust builds. Their docker images are Linux-only (see
            # kallipai-archeion-image / kallipai-lesche-image / kallipai-keryx-image /
            # kallipai-tagma-image below).
            kallipai-archeion = builds.archeion;
            kallipai-admin = builds.admin;
            kallipai-lesche = builds.lesche;
            kallipai-keryx = builds.keryx;
            kallipai-files = builds.files;
            kallipai-tagma = builds.tagma;
            kallipai-run = builds.run;
            kallipai-cron-daemon = builds.cron-daemon;
            kallipai-cron = builds.cron;
            kallipai-daemon = builds.daemon;
            kallipctl = builds.ctl;
            kallipai-daemon-spawn = builds.daemon-spawn;
            # The binaries the NixOS module installs (see nix/packages/workspace.nix).
            kallipai-host = builds.host;
            kallipai-instances = builds.instances;
            kallipai-model-gateway = builds.model-gateway;
            # Retention for the local telemetry stack's data directory (the
            # dev compose's collector/tempo/grafana): see nix/obs-stack.nix.
            kallipai-telemetry-prune = builds.telemetry-prune;
            # The config texts the docker compose stack mounts: the collector,
            # tempo, grafana, the prometheus scrape config and the edge tls
            # snippet, as one directory (see nix/packages/stack.nix).
            kallipai-stack = import ./nix/packages/stack.nix {
              inherit lib pkgs;
            };
            # The FHS distribution tarball builds on the pinned 22.11
            kallipai-tarball = import ./nix/packages/tarball.nix {
              # Tool-side pkgs (patchelf/gnutar) from the main line; the
              # workspace+common come from the pinned instance — the
              # tarball carries the pinned-linker binaries.
              inherit pkgs;
              common = commonFhs;
              inherit (buildsFhs) workspace;
              # Self-containment inputs: the installer script and the
              # unit templates ship beside the binaries in the tarball.
              installScript = ./install.sh;
              systemdTemplates = ./nix/install/systemd;
            };
            # Re-export the per-system-level sharedSkills derivation as a
            # package so deployments can reference the bit-identical store
            # path directly.
            kallipai-shared-skills = sharedSkills;
          }
          # Container images: scratch + nix closure via dockerTools. Linux-only
          # (the buildImage closure is Linux-native). See
          # nix/packages/docker-images/.
          // (lib.optionalAttrs pkgs.stdenv.isLinux {
            # Purpose-built prod images for the split deploy
            # (compose.prod.yaml): archeion, lesche, keryx,
            # and files are the server-side services (co-located, independent images);
            # carries no tagma-specific baked env.
            kallipai-archeion-image = import ./nix/packages/docker-images/archeion.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) archeion admin;
            };
            kallipai-lesche-image = import ./nix/packages/docker-images/lesche.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) lesche;
            };
            kallipai-keryx-image = import ./nix/packages/docker-images/keryx.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) keryx;
            };
            kallipai-files-image = import ./nix/packages/docker-images/files.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) files;
            };
            kallipai-tagma-image = import ./nix/packages/docker-images/tagma.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) tagma;
            };
            # The two remaining platform services and the observability trio,
            # plus the store-less dev image the dev override points every
            # application at: the same shapes as the four above.
            kallipai-model-gateway-image = import ./nix/packages/docker-images/model-gateway.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) model-gateway;
            };
            kallipai-instances-image = import ./nix/packages/docker-images/instances.nix {
              inherit
                pkgs
                common
                ;
              inherit (builds) instances;
            };
            kallipai-otelcol-image = import ./nix/packages/docker-images/otelcol.nix {
              inherit pkgs common;
            };
            kallipai-tempo-image = import ./nix/packages/docker-images/tempo.nix {
              inherit pkgs common;
            };
            kallipai-grafana-image = import ./nix/packages/docker-images/grafana.nix {
              inherit pkgs common;
            };
            # The edge image carries the three Caddyfile entries, the shared site
            # and the SPA bundle; the certificate directory stays a mount. Built here
            # so the image and the flake's bundle cannot disagree about which
            # revision is served.
            kallipai-caddy-web-image = import ./nix/packages/docker-images/caddy-web.nix {
              inherit pkgs common;
              webDist = import ./nix/packages/kallipai-web.nix {
                inherit pkgs;
                src = self;
                inherit (pkgs) deno;
              };
            };
            kallipai-dev-image = import ./nix/packages/docker-images/dev-platform.nix {
              inherit pkgs common;
              inherit (builds)
                archeion
                lesche
                files
                keryx
                model-gateway
                instances
                ;
            };
            # Pre-built integration-test binaries + the agent binaries the
            # suites re-execute. Linux-only like the image.
            kallipai-integration-tests = integrationTests;
            # The image that runs those suites in a container: see
            # scripts/integration-test.sh and compose.test.yaml.
            kallipai-integration-tests-image = import ./nix/packages/docker-images/integration-tests.nix {
              inherit
                pkgs
                common
                integrationTests
                ;
              inherit (integrationTests.passthru) shippedTests;
            };
            # The kallipai-web static site (SPA bundle for a static file
            # server to serve; the NixOS module derives the site root,
            # services.kallipai.web.distWithRuntimeConfig, from it).
            # Linux-only: the node_modules dependency tree carries platform
            # binaries.
            kallipai-web-dist = import ./nix/packages/kallipai-web.nix {
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

          # Opt-in devShell for the kallipai-app Android (Tauri mobile) target.
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
