# Dev files-side composition fragment: files + files-postgres. Imported by
# compose/dev/polis.nix (the default dev stack) -- the files service belongs
# to the archeion-side stack, not its own single-purpose composition, because it
# leans on the archeion's /internal ControlPlane surface over the compose
# network (the same dependency shape as the lesche).
#
# Shape follows the stack's existing services: the workspace `files` binary
# via useHostStore, its own postgres:17.5 with a files_pgdata named volume,
# and the archeion-provisioned internal secret read by file path (the same
# discipline as the lesche/instances pair).
{ pkgs, lib, ... }:
let
  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit --
  # the same resolution compose/dev/polis.nix performs.
  flake = builtins.getFlake "git+file://${toString ../..}";
  workspace = flake.packages.x86_64-linux.default;

  # The files service builds a reqwest Client at startup (the archeion /internal
  # calls), and the rustls platform verifier loads the system trust store
  # EAGERLY at .build() -- so it needs the CA bundle at the standard paths
  # (the shared `cacert` wrapper) even though its /internal calls are plain
  # HTTP. Same reason the lesche service carries the CA layer.
  shared = import ../../nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared) cacert;

  # Host-side publish override, the same env pattern as polis.nix's
  # envOrDefault (the lesche convention): unset -> the default
  # all-interfaces publish on 7400; set -> a second-stack files instance
  # can live beside the first.
  envOrDefault =
    name: default:
    let
      v = builtins.getEnv name;
    in
    if v == "" then default else v;
  filesHostPort = envOrDefault "KALLIPAI_ARION_FILES_PORT" "7400";
  # Bound container stdout logs: the json-file driver caps each service's
  # on-disk log at 50m x 5 rotated files (docker logs reads them). Shared
  # verbatim by every service in this composition.
  logLimits = {
    driver = "json-file";
    options = {
      "max-size" = "50m";
      "max-file" = "5";
    };
  };
in
{
  config = {
    # Named volumes must be declared at the compose top level (compose rejects
    # a reference to an undeclared named volume); declaring them HERE (not in
    # polis.nix) keeps the files-side storage self-contained. The project name
    # prefixes every volume, so the internal name carries the suffix only.
    docker-compose.volumes = {
      files_pgdata = { };
      kallipai_files_blobs = { };
    };

    # Container log rotation caps (50m x 5 json-file per service). The
    # `logging` key has no typed arion option, so it rides the per-service
    # raw `out.service` attrs (the module's documented escape hatch).
    services.files-postgres.out.service.logging = logLimits;
    services.files.out.service.logging = logLimits;
    services.files-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "files_pgdata:/var/lib/postgresql/data" ];
      service.environment = {
        POSTGRES_USER = "kallipai";
        POSTGRES_PASSWORD = "kallipai";
        POSTGRES_DB = "kallipai";
      };
    };

    # Files: the content-transfer service. Content-addressed blobs (local
    # volume) + record metadata in its own Postgres; identity and enrollment
    # facts stay in the archeion, reached through the /internal ControlPlane
    # surface over the compose network. Reached by the `kallip file` CLI
    # (loopback publish) and, since the files page landed, by the
    # browser through the edge's files.<devDomain> vhost.
    services.files = {
      service.depends_on = [
        "archeion"
        "files-postgres"
      ];
      service.useHostStore = true;
      service.command = [ "${workspace}/bin/kallipai-files" ];
      # Loopback-tight publish: the browser path rides the edge vhost;
      # the loopback publish serves the `kallip file` CLI.
      service.ports = [ "127.0.0.1:${filesHostPort}:7400" ];
      service.env_file = [ ".env" ];
      image.contents = [
        workspace
      ]
      ++ cacert;
      service.environment = {
        KALLIPAI_FILES_ADDR = "0.0.0.0:7400";
        KALLIPAI_FILES_DATABASE_URL = "postgres://kallipai:kallipai@files-postgres:5432/kallipai";
        # Blob root inside the container, backed by the named volume below.
        # Service-owned data, not shared with the host daemon tree (unlike
        # the instances binds).
        KALLIPAI_FILES_BLOB_ROOT = "/var/lib/kallipai/files/blobs";
        # Private compose-network hop to the archeion's /internal surface; never
        # routed through the public edge.
        KALLIPAI_FILES_ARCHEION_INTERNAL_URL = "http://archeion:7100";
        # Read the archeion-provisioned internal secret (shared volume).
        KALLIPAI_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        KALLIPAI_FILES_NOTIFY_URL = "http://lesche:7200";
        # Same dev shared secret discipline: must equal the lesche's
        # KALLIPAI_LESCHE_INTERNAL_TOKEN.
        KALLIPAI_FILES_NOTIFY_TOKEN = "dev-notify-secret";
        RUST_LOG = "info";
      };
      service.volumes = [
        "kallipai_files_blobs:/var/lib/kallipai/files/blobs"
        "polis_internal:/var/lib/kallipai/internal:ro"
      ];
    };
  };
}
