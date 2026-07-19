# Arion composition for the prod-agora deploy (the server side): agora +
# postgres. The agora runs from packages.kallip-agora-image; postgres uses the
# official postgres:17 image for production parity and isolation.
#
# Invoke from the repo root (so .env resolves):
#   arion -f nix/prod-composes/agora.nix up -d
#
# This is a single-mode file, so unlike arion-compose.nix there is no
# KALLIP_ARION_MODE switch and no mkIf/mkMerge. ALL deploy env (DB url incl.
# password, WebAuthn RP, CORS, cookie, admin token, POSTGRES_PASSWORD) comes
# from the repo-root .env. The agora is NOT published -- it sits behind a
# TLS-terminating reverse proxy (per crates/kallip-agora/src/args.rs);
# publishing plaintext + COOKIE_SECURE=true would break WebAuthn. See
# docs/reference/container.md.
{ lib, ... }:
let
  # Resolve the workspace flake. `toString ../..` is the repo root (two levels
  # up from this file); the git+file URL applies fetchGit's VCS filtering so the
  # packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  agora = flake.packages.x86_64-linux.kallip-agora;
  agoraImage = flake.packages.x86_64-linux.kallip-agora-image;
in
{
  config = {
    project.name = "kallipai-agora";

    docker-compose.volumes.pgdata = { };

    # The official postgres image. POSTGRES_USER/PASSWORD/DB come from .env
    # ONLY. Do NOT set them in service.environment here -- compose precedence
    # would pin the value and silently ignore the operator's .env, shipping a
    # weak default password on a public DB. Agora retries it with a capped
    # backoff at boot, so `depends_on` (start-order only) is enough -- no
    # healthcheck.
    services.postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "pgdata:/var/lib/postgresql/data" ];
      service.env_file = [ ".env" ];
    };

    services.agora = {
      service.depends_on = [ "postgres" ];
      # arion's image-builder option is `services.<name>.build.image` (a sibling
      # of `service`, not nested under it). mkForce replaces arion's own nix-image
      # builder (which would inject a nix-database layer).
      build.image = lib.mkForce agoraImage;
      service.command = [ "${agora}/bin/kallip-agora" ];
      service.env_file = [ ".env" ];
      service.environment = {
        KALLIP_AGORA_ADDR = "0.0.0.0:7100";
        RUST_LOG = "info";
      };
      # No service.ports -- the agora sits behind a TLS-terminating reverse
      # proxy, which sets X-Forwarded-For; configure
      # KALLIP_AGORA_TRUSTED_PROXIES to the proxy's CIDR.
    };
  };
}
