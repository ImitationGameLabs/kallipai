# Arion composition for the prod-tagma deploy (the host/"tagma" side): a single
# `tagma` service (agent host + in-process relay connector) run from
# packages.kallip-tagma-image.
#
# Invoke from the repo root (so .env resolves):
#   arion -f nix/prod-composes/tagma.nix up -d
#
# This is a single-mode file, so unlike arion-compose.nix there is no
# KALLIP_ARION_MODE switch and no mkIf/mkMerge: every service is declared
# directly. The .env at the repo root supplies KALLIP_AUTH_TOKEN (the tagma
# operator token), KALLIP_RELAY_ENROLLMENT_CODE (first boot only),
# KALLIP_RELAY_AGORA_URL (the prod-agora deploy's public HTTPS URL; ENROLLMENT
# ONLY -- the stored tagma token is reused thereafter), and
# KALLIP_RELAY_LESCHE_URL (the prod-lesche deploy's public HTTPS URL; the tagma
# holds its tunnel here and posts envelopes / key-exchange responses here), and
# the LLM provider credentials. See docs/reference/container.md.
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
    # KALLIP_DATA_DIR/credentials, so they are carried by the `data` volume with
    # no separate mount. Host-dir
    # bind overrides are a dev-only convenience; prod pins storage at the docker
    # layer (data-root) or via a compose edit.
    docker-compose.volumes = {
      data = { };
      workspace = { };
    };

    # The tagma's landlock/seccomp shell sandbox needs these privileges (see
    # docs/reference/container.md). No typed option for security_opt; out.service
    # is the documented escape hatch (attrsOf, merges with the computed spec).
    services.tagma = {
      service.capabilities.SYS_ADMIN = true;
      out.service.security_opt = [ "seccomp=unconfined" ];
      service.ports = [ "3000:3000" ];
      service.volumes = [
        "data:/var/lib/kallip"
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
      # unreachable agora it degrades to local-only (logs an error, keeps serving
      # local agents, the lesche message route returns 503).
      # `restart: unless-stopped`
      # brings it back once the code is supplied / the agora is reachable.
      service.environment = {
        HOME = "/var/lib/kallip";
        KALLIP_DATA_DIR = "/var/lib/kallip";
        KALLIP_WORKSPACE_ROOT = "/workspace";
        KALLIP_TAGMA_ADDR = "0.0.0.0:3000";
        RUST_LOG = "info";
        # KALLIP_AUTH_TOKEN (operator token), KALLIP_RELAY_ENROLLMENT_CODE (first
        # run only), KALLIP_RELAY_AGORA_URL (enroll-only), and
        # KALLIP_RELAY_LESCHE_URL (tunnel + envelopes + KEX responses) come from
        # .env. Per the per-service subdomain topology the agora and lesche are
        # two distinct origins (e.g. https://agora.kallipai.com and
        # https://lesche.kallipai.com) sharing the parent domain.
      };
    };
  };
}
