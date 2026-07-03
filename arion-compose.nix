# Arion composition for just-agent-daemon.
#
# Two modes, switched by the JUST_AGENT_ARION_MODE env var. Dev is the default
# (cheap iteration via useHostStore); production is opt-in:
#
#   arion up                                  # dev (default)
#   JUST_AGENT_ARION_MODE=prod arion up       # production
#
# Both modes consume the flake's pre-built outputs directly — arion does no
# Rust/crane building of its own:
#   - dev:  packages.default (the crane workspace), run via useHostStore so the
#           host /nix/store is shared and `nix build .#default` picks up changes.
#   - prod: packages.just-agent-image, handed to arion via build.image so arion
#           loads and runs it in one command.
{ pkgs, lib, ... }:
let
  isProd = builtins.getEnv "JUST_AGENT_ARION_MODE" == "prod";

  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ./.}";
  workspace = flake.packages.x86_64-linux.default;
  image = flake.packages.x86_64-linux.just-agent-image;

  # Shared toolset + certs + aifed + PATH (same recipe as the baked image in
  # nix/packages/container-shared.nix), so dev and prod cannot drift.
  shared = import ./nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    certLinks
    aifed
    binPath
    ;
in
{
  config = lib.mkMerge [
    {
      project.name = "just-agent";

      services.just-agent = {
        # Common to both modes: the landlock/seccomp shell sandbox needs these
        # privileges (see docs/reference/container.md), the daemon port, persistent
        # state + workspace, and .env for provider credentials.
        service.capabilities.SYS_ADMIN = true;
        service.ports = [ "3000:3000" ];
        service.volumes = [
          "${toString ./.}/data:/var/lib/just-agent"
          "${toString ./.}/ws:/workspace"
        ];
        service.env_file = [ ".env" ];
        # No typed option for security_opt; out.service is the documented escape
        # hatch (attrsOf, merges with the computed service spec).
        out.service.security_opt = [ "seccomp=unconfined" ];
      };
    }
    (lib.mkIf isProd {
      # Hand arion the flake-built image: it loads it and derives the tag, so
      # `JUST_AGENT_ARION_MODE=prod arion up` builds + runs in one command (no
      # manual `docker load`). mkForce overrides arion's own image builder,
      # which would otherwise inject a nix-database layer (and pull pkgs.nix
      # into the closure).
      services.just-agent.build.image = lib.mkForce image;
    })
    (lib.mkIf (!isProd) {
      services.just-agent = {
        # Adds root-level /bin/sh and /usr/bin/env symlinks. The daemon and agent
        # shells don't need them (bash is resolved via PATH/toolEnv); this is a
        # dev-only convenience for `arion exec` and any `#!/bin/sh` shebangs.
        image.enableRecommendedContents = true;
        image.contents = [
          workspace
          toolEnv
          pkgs.cacert
          certLinks
          aifed
        ];
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
  ];
}
