# Arion composition for just-agent-daemon.
#
# Three modes, switched by the JUST_AGENT_ARION_MODE env var. Dev is the default
# (cheap iteration via useHostStore); production and test are opt-in:
#
#   arion up                                  # dev (default)
#   JUST_AGENT_ARION_MODE=prod arion up       # production (baked image)
#   JUST_AGENT_ARION_MODE=test arion up       # run the integration-test suite
#
# All modes consume the flake's pre-built outputs directly — arion does no
# Rust/crane building of its own:
#   - dev:  packages.default (the crane workspace), run via useHostStore so the
#           host /nix/store is shared and `nix build .#default` picks up changes.
#   - prod: packages.just-agent-image, handed to arion via build.image so arion
#           loads and runs it in one command.
#   - test: packages.just-agent-integration-tests (every [[test]] binary + the
#           agent binaries), run via useHostStore; the service exits with the
#           test verdict.
{ pkgs, lib, ... }:
let
  mode = builtins.getEnv "JUST_AGENT_ARION_MODE";
  isProd = mode == "prod";
  isTest = mode == "test";
  # Dev is the default: any unset / unrecognized value (including "") runs dev.
  isDev = !isProd && !isTest;

  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ./.}";
  workspace = flake.packages.x86_64-linux.default;
  image = flake.packages.x86_64-linux.just-agent-image;
  integrationTests = flake.packages.x86_64-linux.just-agent-integration-tests;

  # Shared toolset + certs + aifed + PATH (same recipe as the baked image in
  # nix/packages/container-shared.nix), so dev and prod cannot drift.
  shared = import ./nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    certLinks
    aifed
    binPath
    ;

  # The runtime image tail common to dev and test (the mode-specific package --
  # `workspace` or `integrationTests` -- is prepended by each branch). Mirrors
  # the base layer of nix/packages/container-image.nix.
  runtimeContents = [
    toolEnv
    pkgs.cacert
    certLinks
    aifed
  ];
in
{
  config = lib.mkMerge [
    {
      project.name = "just-agent";

      services.just-agent = {
        # Common to every mode: the landlock/seccomp shell sandbox needs these
        # privileges (see docs/reference/container.md). No typed option for
        # security_opt; out.service is the documented escape hatch (attrsOf,
        # merges with the computed service spec).
        service.capabilities.SYS_ADMIN = true;
        out.service.security_opt = [ "seccomp=unconfined" ];
      };
    }
    # Dev and prod host the long-running daemon: expose the port, persist state
    # + workspace, and read provider credentials from .env. The test mode runs
    # an ephemeral suite (in-process wiremock, internal ephemeral ports) and
    # needs none of these.
    (lib.mkIf (!isTest) {
      services.just-agent = {
        service.ports = [ "3000:3000" ];
        service.volumes = [
          "${toString ./.}/data:/var/lib/just-agent"
          "${toString ./.}/ws:/workspace"
        ];
        service.env_file = [ ".env" ];
      };
    })
    (lib.mkIf isProd {
      # Hand arion the flake-built image: it loads it and derives the tag, so
      # `JUST_AGENT_ARION_MODE=prod arion up` builds + runs in one command (no
      # manual `docker load`). mkForce overrides arion's own image builder,
      # which would otherwise inject a nix-database layer (and pull pkgs.nix
      # into the closure).
      services.just-agent.build.image = lib.mkForce image;
    })
    (lib.mkIf isDev {
      services.just-agent = {
        # Adds root-level /bin/sh and /usr/bin/env symlinks. The daemon and
        # agent shells don't need them (bash is resolved via PATH/toolEnv); this
        # is a convenience for `arion exec` and any `#!/bin/sh` shebangs (the
        # test mode also relies on /bin/sh for its iterate script).
        image.enableRecommendedContents = true;
        image.contents = [ workspace ] ++ runtimeContents;
        service.useHostStore = true;
        service.command = [ "${workspace}/bin/just-agent-daemon" ];
        service.environment = {
          PATH = binPath;
          HOME = "/var/lib/just-agent";
          JUST_AGENT_DATA_DIR = "/var/lib/just-agent";
          JUST_AGENT_DAEMON_ADDR = "0.0.0.0:3000";
          RUST_LOG = "info";
        };
      };
    })
    (lib.mkIf isTest {
      services.just-agent = {
        image.enableRecommendedContents = true;
        image.contents = [ integrationTests ] ++ runtimeContents;
        service.useHostStore = true;
        # Run every pre-built [[test]] binary. --nocapture surfaces the
        # scenario eprintln! diagnostics in `arion logs`; each libtest harness
        # exits non-zero on failure, so `arion ps -a` reports the overall
        # verdict. `set -e` makes the loop fail-fast across binaries (an early
        # failure masks later ones; order is alphabetical: exec, then sandbox).
        # The `[ -e ]` guard handles an empty glob; `found` refuses to silently
        # pass when no test binary is present. /bin/sh comes from
        # enableRecommendedContents. restart = "no" so the exited container
        # isn't restarted.
        # Caveat: --nocapture assumes each binary is a libtest harness; a future
        # [[test]] with `harness = false` would reject the flag and fail fast.
        service.command = [
          "/bin/sh"
          "-c"
          ''
            set -e
            found=0
            for t in /integration-tests/*; do
              [ -e "$t" ] || continue
              found=1
              echo "=== integration test: $t ==="
              "$t" --nocapture
            done
            [ "$found" = 1 ] || { echo "no integration tests found"; exit 1; }
          ''
        ];
        service.restart = "no";
        # /testdata holds the sandbox scenarios' home/data/workspace scratch
        # dirs. It MUST be outside libsandbox's baseline-writable set (/tmp,
        # /var/tmp, $TMPDIR) so write-denial assertions stay honest; a dedicated
        # tmpfs is the simplest such path. Same escape hatch as security_opt.
        out.service.tmpfs = [ "/testdata:rw,size=64m" ];
        service.environment = {
          PATH = "${integrationTests}/bin:${binPath}";
          # Explicit agent-bin dir for resolve_bin -- current_exe() resolves
          # the buildEnv symlink into a sub-store path, not the shared bin/.
          JUST_AGENT_BIN_DIR = "${integrationTests}/bin";
          JUST_AGENT_TESTDATA_DIR = "/testdata";
          HOME = "/var/lib/just-agent";
          RUST_LOG = "info";
        };
      };
    })
  ];
}
