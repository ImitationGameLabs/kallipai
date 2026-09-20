# Arion composition for the prod-tagma deploy (the host/"tagma" side): a single
# `tagma` service (agent host + in-process relay connector) run from
# packages.kallip-tagma-image.
#
# Invoke from the repo root (so .env resolves):
#   arion -f compose/prod/tagma.nix up -d
#
# This is a single-purpose file: every service is declared directly, no mode
# switch or mkIf/mkMerge. The .env at the repo root supplies KALLIP_AUTH_TOKEN (the tagma
# operator token), KALLIP_TAGMA_RELAY_ENROLLMENT_CODE (first boot only),
# KALLIP_POLIS_URL (the platform's public API origin, e.g.
# https://api.kallipai.com -- enrollment, the lesche tunnel, envelopes and
# key-exchange responses all derive from it), and
# the LLM provider credentials. See docs/en/deployment/container.md.
{ lib, ... }:
let
  # Resolve the workspace flake. `toString ../..` is the repo root (two levels
  # up from this file); the git+file URL applies fetchGit's VCS filtering so the
  # packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  tagma = flake.packages.x86_64-linux.kallip-tagma;
  tagmaImage = flake.packages.x86_64-linux.kallip-tagma-image;
in
{
  config = {
    project.name = "kallipai-tagma";

    # Tagma data and the agent workspace live in docker named volumes
    # (persistent; survive `arion down`, removed by `arion down -v`). The tagma
    # credentials (device key + tagma token) live under
    # the instance data root's credentials/, so they are carried by the `data` volume with
    # no separate mount. Host-dir
    # bind overrides are a dev-only convenience; prod pins storage at the docker
    # layer (data-root) or via a compose edit.
    docker-compose.volumes = {
      kallipai_tagma_data = { };
      workspace = { };
    };

    # The tagma's landlock/seccomp shell sandbox needs these privileges (see
    # docs/en/deployment/container.md). No typed option for security_opt; out.service
    # is the documented escape hatch (attrsOf, merges with the computed spec).
    services.tagma = {
      service.capabilities.SYS_ADMIN = true;
      out.service.security_opt = [ "seccomp=unconfined" ];
      service.ports = [ "3000:3000" ];
      service.volumes = [
        "kallipai_tagma_data:/var/lib/kallipai/tagmata/main"
        "workspace:/workspace"
      ];
      service.env_file = [ ".env" ];
      # arion's image-builder option is `services.<name>.build.image` (a sibling
      # of `service`, not nested under it). mkForce replaces arion's own nix-image
      # builder (which would inject a nix-database layer).
      build.image = lib.mkForce tagmaImage;
      service.command = [ "${tagma}/bin/kallip-tagma" ];
      service.restart = "unless-stopped";
      # The relay's first-boot enroll() is NOT retried in code: on a missing or
      # unreachable archeion it degrades to local-only (logs an error, keeps serving
      # local agents, the lesche message route returns 503).
      # `restart: unless-stopped`
      # brings it back once the code is supplied / the archeion is reachable.
      service.environment = {
        HOME = "/var/lib";
        # Slug boot: the data root derives from KALLIP_TAGMA_SLUG under the XDG
        # data home - /var/lib/kallipai/tagmata/main, exactly the mounted
        # data volume; logs land inside the volume as well.
        KALLIP_TAGMA_SLUG = "main";
        XDG_DATA_HOME = "/var/lib";
        XDG_STATE_HOME = "/var/lib";
        KALLIP_WORKSPACE_ROOT = "/workspace";
        KALLIP_TAGMA_ADDR = "0.0.0.0:3000";
        # A rootful container has no uid mapping, so the tagma would run as the
        # host's real root -- which the real-root boot guard refuses by default
        # (docs/en/reference/env/daemon.md). This flag is the explicit, documented escape:
        # instances inherit the container's root. Rootless or userns-remap
        # deployments remain the preferred shapes.
        KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT = "1";
        RUST_LOG = "info";
        # KALLIP_AUTH_TOKEN (operator token), KALLIP_TAGMA_RELAY_ENROLLMENT_CODE
        # (first run only), and KALLIP_POLIS_URL (the platform API origin:
        # enrollment at the archeion, the lesche tunnel, envelopes and KEX
        # responses all derive from it) come from .env. The api face serves
        # every service under https://api.<domain>/v1/<service>/... on one
        # origin sharing the parent domain with app.<domain>.
      };
    };
  };
}
