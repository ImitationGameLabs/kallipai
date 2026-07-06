# Arion composition for kallip-daemon.
#
# Three modes, switched by the KALLIP_ARION_MODE env var. Dev is the default
# (cheap iteration via useHostStore); production and test are opt-in:
#
#   arion up                                  # dev (default)
#   KALLIP_ARION_MODE=prod arion up       # production (baked image)
#   KALLIP_ARION_MODE=test arion up       # run the integration-test suite
#
# All modes consume the flake's pre-built outputs directly — arion does no
# Rust/crane building of its own:
#   - dev:  packages.default (the crane workspace), run via useHostStore so the
#           host /nix/store is shared and `nix build .#default` picks up changes.
#   - prod: packages.kallip-image, handed to arion via build.image so arion
#           loads and runs it in one command.
#   - test: packages.kallip-integration-tests (every [[test]] binary + the
#           agent binaries), run via useHostStore; the service exits with the
#           test verdict.
{ pkgs, lib, ... }:
let
  mode = builtins.getEnv "KALLIP_ARION_MODE";
  isProd = mode == "prod";
  isTest = mode == "test";
  # Dev is the default: any unset / unrecognized value (including "") runs dev.
  isDev = !isProd && !isTest;

  # Read a bind-override env var, returning "<host>:<target>" or null when
  # unset. The value must be empty or an absolute, colon-free path other than
  # "/": a bare name is parsed by compose as a named-volume ref (-> "undefined
  # volume"), a colon lets docker mis-parse the src:dst[:mode] string and
  # silently mount to the wrong target (-> data loss on arion down), and "/"
  # would bind the host root into the container. Throws at eval in dev/prod;
  # in test mode the bindings are lazily ignored (never referenced), so a bad
  # value is silent there.
  bindOverride =
    name: target:
    let
      v = builtins.getEnv name;
    in
    if v == "" then
      null
    else if v == "/" || !(lib.hasPrefix "/" v) || lib.hasInfix ":" v then
      throw "arion: ${name} must be an absolute, colon-free host path other than '/' (got '${v}')"
    else
      "${v}:${target}";

  # Bind overrides for the daemon data, agent workspace, and shared skills.
  # Unset (the default) backs data + workspace with docker named volumes and
  # leaves skills living inside the data volume; set to a host path to
  # bind-mount instead -- data to keep daemon state on a known disk, workspace
  # to make the agent's files host-visible, skills to curate shared skills on
  # the host.
  dataBind = bindOverride "KALLIP_ARION_DATA_PATH" "/var/lib/kallip";
  workspaceBind = bindOverride "KALLIP_ARION_WORKSPACE_PATH" "/workspace";
  # Skills overlay the data volume's skills/ subdir (no named volume of its
  # own). NOTE: KALLIP_SKILLS_ROOT (if set via .env) short-circuits
  # skill_dir() and bypasses this bind, so leave it unset when using it.
  skillsBind = bindOverride "KALLIP_ARION_SKILLS_PATH" "/var/lib/kallip/skills";

  dataVolume = if dataBind != null then dataBind else "data:/var/lib/kallip";
  workspaceVolume = if workspaceBind != null then workspaceBind else "workspace:/workspace";

  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ./.}";
  workspace = flake.packages.x86_64-linux.default;
  image = flake.packages.x86_64-linux.kallip-image;
  integrationTests = flake.packages.x86_64-linux.kallip-integration-tests;

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
      project.name = "kallip";

      services.kallip = {
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
    # needs none of these. Daemon data and the workspace are docker named
    # volumes by default (no host directories are created in the project tree);
    # either can be bind-mounted via KALLIP_ARION_DATA_PATH /
    # KALLIP_ARION_WORKSPACE_PATH, and shared skills can be overlaid on the
    # data volume via KALLIP_ARION_SKILLS_PATH.
    (lib.mkIf (!isTest) {
      # Named volumes must be declared at the compose top level; compose rejects
      # a service reference to an undeclared named volume. Each is declared only
      # when its override env var is unset (otherwise that mount is a bind mount
      # and no named volume is referenced).
      docker-compose.volumes =
        { }
        // lib.optionalAttrs (dataBind == null) { data = { }; }
        // lib.optionalAttrs (workspaceBind == null) { workspace = { }; };
      services.kallip = {
        service.ports = [ "3000:3000" ];
        service.volumes = [
          dataVolume
          workspaceVolume
        ]
        # skills has no named volume of its own: unset -> skills live inside
        # the `data` volume's skills/ subdir; set -> a bind overlays it.
        ++ lib.optional (skillsBind != null) skillsBind;
        service.env_file = [ ".env" ];
      };
    })
    (lib.mkIf isProd {
      # Hand arion the flake-built image: it loads it and derives the tag, so
      # `KALLIP_ARION_MODE=prod arion up` builds + runs in one command (no
      # manual `docker load`). mkForce overrides arion's own image builder,
      # which would otherwise inject a nix-database layer (and pull pkgs.nix
      # into the closure).
      services.kallip.build.image = lib.mkForce image;
    })
    (lib.mkIf isDev {
      services.kallip = {
        # Adds root-level /bin/sh and /usr/bin/env symlinks. The daemon and
        # agent shells don't need them (bash is resolved via PATH/toolEnv); this
        # is a convenience for `arion exec` and any `#!/bin/sh` shebangs (the
        # test mode also relies on /bin/sh for its iterate script).
        image.enableRecommendedContents = true;
        image.contents = [ workspace ] ++ runtimeContents;
        service.useHostStore = true;
        service.command = [ "${workspace}/bin/kallip-daemon" ];
        service.environment = {
          PATH = binPath;
          HOME = "/var/lib/kallip";
          KALLIP_DATA_DIR = "/var/lib/kallip";
          # Default workspace for clients (e.g. the TUI) that create an agent
          # without an explicit workspace_root: AgentConfig::load otherwise
          # falls back to the daemon's cwd, which is "/" in the container and
          # overlaps the data dir -> 409. Pin the mounted workspace volume,
          # which is disjoint from /var/lib/kallip.
          KALLIP_WORKSPACE_ROOT = "/workspace";
          KALLIP_DAEMON_ADDR = "0.0.0.0:3000";
          RUST_LOG = "info";
        };
      };
    })
    (lib.mkIf isTest {
      services.kallip = {
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
          KALLIP_BIN_DIR = "${integrationTests}/bin";
          KALLIP_TESTDATA_DIR = "/testdata";
          HOME = "/var/lib/kallip";
          RUST_LOG = "info";
        };
      };
    })
  ];
}
