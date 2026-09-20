# Arion composition for the prod-archeion deploy (the server side): archeion +
# lesche + files + archeion-postgres + lesche-postgres + files-postgres. The
# archeion (control plane) runs from packages.kallip-archeion-image; the lesche
# (data-plane relay) runs from packages.kallip-lesche-image; files (content
# transfer) runs from packages.kallip-files-image; each postgres uses the
# official postgres:17.5 image for production parity and isolation.
#
# Invoke from the repo root (so .env resolves):
#   arion -f compose/prod/polis.nix up -d
#
# This is a single-purpose file: every service is declared directly, no mode
# switch or mkIf/mkMerge. Secret-bearing deploy env (DB url incl.
# password, WebAuthn RP, CORS, cookie domain, admin token, the internal
# shared secret, POSTGRES_PASSWORD) comes from the repo-root .env; each
# service's operational env (listen addr, blob root, internal hop URL)
# is pinned inline and overrides env_file.
# None of the three services is published -- all sit behind the
# operator's TLS-terminating edge proxy, which HOST-routes archeion.<d> -> archeion
# and lesche.<d> -> lesche / files.<d> -> files (the per-service subdomain
# topology). The lesche and the files service reach the archeion's /internal
# ControlPlane surface over the private compose network (each with its own
# KALLIP_*_ARCHEION_INTERNAL_URL=http://archeion:7100); the proxy must NOT route
# /internal publicly. See docs/en/deployment/container.md.
{ lib, ... }:
let
  # Resolve the workspace flake. `toString ../..` is the repo root (two levels
  # up from this file); the git+file URL applies fetchGit's VCS filtering so the
  # packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  archeion = flake.packages.x86_64-linux.kallip-archeion;
  archeionImage = flake.packages.x86_64-linux.kallip-archeion-image;
  lesche = flake.packages.x86_64-linux.kallip-lesche;
  lescheImage = flake.packages.x86_64-linux.kallip-lesche-image;
  files = flake.packages.x86_64-linux.kallip-files;
  filesImage = flake.packages.x86_64-linux.kallip-files-image;
in
{
  config = {
    project.name = "kallipai-archeion";

    docker-compose.volumes = {
      archeion_pgdata = { };
      lesche_pgdata = { };
      files_pgdata = { };
      kallipai_files_blobs = { };
      # The archeion-provisioned internal secret: written by the archeion
      # (rw), read by the lesche and files (ro). Survives restarts, so the
      # value stays stable for the whole composition.
      polis_internal = { };
    };

    # POSTGRES_USER/PASSWORD/DB come from .env ONLY and are read by all
    # three postgres services -- do NOT set them in service.environment
    # (compose precedence would pin a weak default password on a public DB).
    services.archeion-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "archeion_pgdata:/var/lib/postgresql/data" ];
      service.env_file = [ ".env" ];
    };

    services.lesche-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "lesche_pgdata:/var/lib/postgresql/data" ];
      service.env_file = [ ".env" ];
    };

    services.files-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "files_pgdata:/var/lib/postgresql/data" ];
      service.env_file = [ ".env" ];
    };

    services.archeion = {
      service.depends_on = [ "archeion-postgres" ];
      # arion's image-builder option is `services.<name>.build.image` (a sibling
      # of `service`, not nested under it). mkForce replaces arion's own nix-image
      # builder (which would inject a nix-database layer).
      build.image = lib.mkForce archeionImage;
      service.command = [ "${archeion}/bin/kallip-archeion" ];
      service.env_file = [ ".env" ];
      # The internal secret is self-managed: first boot generates it into
      # the shared volume, later boots read the existing value; the lesche
      # and files read the same file (mounted read-only below).
      service.volumes = [ "polis_internal:/var/lib/kallipai/internal" ];
      service.environment = {
        KALLIP_ARCHEION_ADDR = "0.0.0.0:7100";
        KALLIP_ARCHEION_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        RUST_LOG = "info";
        # KALLIP_ARCHEION_SESSION_COOKIE_DOMAIN comes from .env: set to the parent
        # KALLIP_ARCHEION_SESSION_COOKIE_DOMAIN comes from .env: set to the parent
        # domain (e.g. kallipai.com) so the session cookie is shared across the
        # archeion.<d> and lesche.<d> subdomains the edge routes here.
      };
      # No service.ports -- the archeion sits behind the operator's TLS-terminating
      # edge proxy, which HOST-routes archeion.<d> -> archeion:7100 and lesche.<d> ->
      # lesche:7200 and sets X-Forwarded-For; configure
      # KALLIP_ARCHEION_TRUSTED_PROXIES to the proxy's CIDR (prod keeps its proxy,
      # unlike dev). /internal is reached by the lesche over the private compose
      # network, never via the public edge.
    };

    # Lesche: the data-plane relay (tagma relay tunnels, app SSE, envelope routing,
    # KEX, presence). Owns the chat domain in its own Postgres (rooms, membership,
    # message payloads); it authenticates requests and
    # attests identity through the archeion's /internal ControlPlane API over the
    # private compose network. Not published -- the operator's edge host-routes
    # lesche.<d> here.
    services.lesche = {
      # No healthcheck; unlike the archeion, lesche does not retry its DB connect.
      service.depends_on = [
        "archeion"
        "lesche-postgres"
      ];
      build.image = lib.mkForce lescheImage;
      service.command = [ "${lesche}/bin/kallip-lesche" ];
      service.env_file = [ ".env" ];
      service.volumes = [ "polis_internal:/var/lib/kallipai/internal:ro" ];
      service.environment = {
        KALLIP_LESCHE_ADDR = "0.0.0.0:7200";
        # Private compose-network hop to the archeion's /internal surface; never
        # routed through the public edge.
        KALLIP_LESCHE_ARCHEION_INTERNAL_URL = "http://archeion:7100";
        # Read the archeion-provisioned internal secret (shared volume).
        KALLIP_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        RUST_LOG = "info";
        # KALLIP_LESCHE_DATABASE_URL (the chat schema) and
        # KALLIP_LESCHE_CORS_ORIGINS come from .env.
      };
      # No service.ports -- like the archeion, the lesche sits behind the
      # TLS-terminating reverse proxy.
    };

    # Files: the content-transfer service. Content-addressed blobs (local
    # volume) + record metadata in its own Postgres; identity and enrollment
    # facts stay in the archeion, verified per request through the archeion's
    # /internal ControlPlane surface over the private compose network. Not
    # published -- the operator's edge host-routes files.<d> here.
    services.files = {
      service.depends_on = [
        "archeion"
        "files-postgres"
      ];
      build.image = lib.mkForce filesImage;
      service.command = [ "${files}/bin/kallip-files" ];
      service.env_file = [ ".env" ];
      service.volumes = [
        "kallipai_files_blobs:/var/lib/kallipai/files/blobs"
        "polis_internal:/var/lib/kallipai/internal:ro"
      ];
      service.environment = {
        KALLIP_FILES_ADDR = "0.0.0.0:7400";
        # The blob root INSIDE the container; must equal the kallipai_files_blobs
        # volume mount target above.
        KALLIP_FILES_BLOB_ROOT = "/var/lib/kallipai/files/blobs";
        # Private compose-network hop to the archeion's /internal surface;
        # never routed through the public edge.
        KALLIP_FILES_ARCHEION_INTERNAL_URL = "http://archeion:7100";
        RUST_LOG = "info";
        # KALLIP_FILES_DATABASE_URL (the metadata schema) comes from .env;
        # the internal secret is read from the archeion-provisioned file
        # (KALLIP_POLIS_INTERNAL_TOKEN_FILE above).
      };
      # No service.ports -- like the archeion and the lesche, the files service
      # sits behind the TLS-terminating reverse proxy.
    };
  };
}
