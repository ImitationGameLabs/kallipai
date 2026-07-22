# Arion composition for the prod-agora deploy (the server side): agora +
# lesche + postgres. agora (control plane) runs from packages.kallip-agora-image;
# lesche (data-plane relay) runs from packages.kallip-lesche-image; postgres
# uses the official postgres:17 image for production parity and isolation.
#
# Invoke from the repo root (so .env resolves):
#   arion -f nix/prod-composes/agora.nix up -d
#
# This is a single-mode file, so unlike arion-compose.nix there is no
# KALLIP_ARION_MODE switch and no mkIf/mkMerge. ALL deploy env (DB url incl.
# password, WebAuthn RP, CORS, cookie domain, admin token, the agora/lesche
# internal shared secret, POSTGRES_PASSWORD) comes from the repo-root .env.
# Neither the agora nor the lesche is published -- both sit behind the
# operator's TLS-terminating edge proxy, which HOST-routes agora.<d> -> agora
# and lesche.<d> -> lesche (the per-service subdomain topology). The lesche
# reaches the agora's /internal ControlPlane surface over the private
# compose network (KALLIP_LESCHE_AGORA_INTERNAL_URL=http://agora:7100); the
# proxy must NOT route /internal publicly. See docs/reference/container.md.
{ lib, ... }:
let
  # Resolve the workspace flake. `toString ../..` is the repo root (two levels
  # up from this file); the git+file URL applies fetchGit's VCS filtering so the
  # packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  agora = flake.packages.x86_64-linux.kallip-agora;
  agoraImage = flake.packages.x86_64-linux.kallip-agora-image;
  lesche = flake.packages.x86_64-linux.kallip-lesche;
  lescheImage = flake.packages.x86_64-linux.kallip-lesche-image;
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
        # KALLIP_AGORA_INTERNAL_TOKEN (the shared secret the lesche presents to
        # the /internal/* surface) comes from .env. When unset, the agora runs
        # standalone and the /internal nest is not mounted -- so the lesche
        # service below will fail its ControlPlane calls until it is set.
        # KALLIP_AGORA_SESSION_COOKIE_DOMAIN comes from .env: set to the parent
        # domain (e.g. kallipai.com) so the session cookie is shared across the
        # agora.<d> and lesche.<d> subdomains the edge routes here.
      };
      # No service.ports -- the agora sits behind the operator's TLS-terminating
      # edge proxy, which HOST-routes agora.<d> -> agora:7100 and lesche.<d> ->
      # lesche:7200 and sets X-Forwarded-For; configure
      # KALLIP_AGORA_TRUSTED_PROXIES to the proxy's CIDR (prod keeps its proxy,
      # unlike dev). /internal is reached by the lesche over the private compose
      # network, never via the public edge.
    };

    # Lesche: the data-plane relay (herald tunnels, app SSE, envelope routing,
    # KEX, presence). DB-free; it authenticates requests and resolves tagma
    # metadata through the agora's /internal ControlPlane API over the private
    # compose network. Not published -- the operator's edge host-routes
    # lesche.<d> here.
    services.lesche = {
      service.depends_on = [ "agora" ];
      build.image = lib.mkForce lescheImage;
      service.command = [ "${lesche}/bin/kallip-lesche" ];
      service.env_file = [ ".env" ];
      service.environment = {
        KALLIP_LESCHE_ADDR = "0.0.0.0:7200";
        # Private compose-network hop to the agora's /internal surface; never
        # routed through the public edge.
        KALLIP_LESCHE_AGORA_INTERNAL_URL = "http://agora:7100";
        RUST_LOG = "info";
        # KALLIP_LESCHE_AGORA_TOKEN (must equal the agora's
        # KALLIP_AGORA_INTERNAL_TOKEN) + KALLIP_LESCHE_CORS_ORIGINS come from
        # .env.
      };
      # No service.ports -- like the agora, the lesche sits behind the
      # TLS-terminating reverse proxy.
    };
  };
}
