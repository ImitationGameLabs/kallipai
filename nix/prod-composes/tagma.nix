# Arion composition for the prod-tagma deploy (the host/"tagma" side):
# daemon + herald, both run from packages.kallip-tagma-image.
#
# Invoke from the repo root (so .env resolves):
#   arion -f nix/prod-composes/tagma.nix up -d
#
# This is a single-mode file, so unlike arion-compose.nix there is no
# KALLIP_ARION_MODE switch and no mkIf/mkMerge: every service is declared
# directly. The .env at the repo root supplies KALLIP_AUTH_TOKEN (the daemon
# operator token the herald presents), KALLIP_HERALD_ENROLLMENT_CODE (first
# boot only), KALLIP_HERALD_AGORA_URL (the prod-agora deploy's public HTTPS
# URL; ENROLLMENT ONLY -- the stored tagma token is reused thereafter),
# KALLIP_HERALD_LESCHE_URL (the prod-lesche deploy's public HTTPS URL; the
# herald holds its tunnel here and posts envelopes / key-exchange responses
# here), and the LLM provider credentials. See docs/reference/container.md.
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

    # Daemon data, the agent workspace, and the herald's device key all live in
    # docker named volumes (persistent; survive `arion down`, removed by `arion
    # down -v`). Host-dir bind overrides are a dev-only convenience; prod pins
    # storage at the docker layer (data-root) or via a compose edit.
    docker-compose.volumes = {
      data = { };
      workspace = { };
      herald-state = { };
    };

    # The daemon's landlock/seccomp shell sandbox needs these privileges (see
    # docs/reference/container.md). No typed option for security_opt; out.service
    # is the documented escape hatch (attrsOf, merges with the computed spec).
    services.daemon = {
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
      service.command = [ "${tagma}/bin/kallip-daemon" ];
      service.environment = {
        # The image bakes only PATH; the daemon-specific env lives here so it
        # does not leak into the herald service.
        HOME = "/var/lib/kallip";
        KALLIP_DATA_DIR = "/var/lib/kallip";
        KALLIP_WORKSPACE_ROOT = "/workspace";
        KALLIP_DAEMON_ADDR = "0.0.0.0:3000";
        RUST_LOG = "info";
      };
    };

    # The herald persists its device key + tagma token in `herald-state`, so it
    # re-enrolls only on the very first boot (using
    # KALLIP_HERALD_ENROLLMENT_CODE from .env, minted via the agora dashboard).
    # Its first-boot enroll() is NOT retried in code, so `restart:
    # unless-stopped` lets it come back once the code is supplied / the agora is
    # reachable (a bad code crashloops -- documented). The daemon is co-located
    # in this composition; the agora is the separate prod-agora deploy, reached
    # at its public HTTPS URL via KALLIP_HERALD_AGORA_URL from .env.
    services.herald = {
      # mkForce replaces arion's own nix-image builder (which would inject a
      # nix-database layer); herald shares the tagma image with the daemon.
      build.image = lib.mkForce tagmaImage;
      service.command = [ "${tagma}/bin/kallip-herald" ];
      service.volumes = [ "herald-state:/var/lib/kallip/herald" ];
      service.restart = "unless-stopped";
      # KALLIP_AUTH_TOKEN (herald -> daemon operator token) +
      # KALLIP_HERALD_ENROLLMENT_CODE (first run only).
      service.env_file = [ ".env" ];
      service.depends_on = [ "daemon" ];
      service.environment = {
        KALLIP_HERALD_STATE_DIR = "/var/lib/kallip/herald";
        KALLIP_DAEMON_URL = "http://daemon:3000";
        RUST_LOG = "info";
        # KALLIP_HERALD_AGORA_URL (enroll-only) and KALLIP_HERALD_LESCHE_URL
        # (tunnel + envelopes + KEX responses) are intentionally NOT set here --
        # both come from .env. Per the per-service subdomain topology these are
        # two distinct origins (e.g. https://agora.kallipai.com and
        # https://lesche.kallipai.com) sharing the parent domain.
      };
    };
  };
}
