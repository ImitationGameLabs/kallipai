# Dev archeion-side composition: caddy + archeion + lesche + files +
# archeion-postgres + lesche-postgres + files-postgres + instances. The default
# dev stack -- a plain `arion up` brings it up via the `arion-compose.nix` shim
# at the repo root (which just re-exports this module); invoke directly with
# `arion -f compose/dev/polis.nix ...` for the same result.
#
# The dev tagma (compose/dev/tagma.nix) and the integration-test runner
# (compose/dev/test.nix) are NOT here: each is its own single-purpose
# composition under compose/dev/, sharing nothing with the archeion side.
# The files service is composed here too, imported from files.nix:
# it belongs to this stack, reaching the archeion's /internal surface
# over the compose network (the same dependency shape as the lesche).
# Prod-tagma / prod-archeion are standalone under compose/prod/.
#
# Consumes the flake's pre-built `packages.default` directly -- arion does no
# Rust/crane building. `useHostStore` shares the host /nix/store into the
# containers, so a rebuild is picked up without an in-compose bake. See
# docs/development.md for the bring-up commands and flow.
{ pkgs, lib, ... }:
let
  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved packages match `nix build .#*` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  workspace = flake.packages.x86_64-linux.default;

  # Shared toolset + certs + aifed + PATH. Reused by the tagma docker image
  # (nix/packages/docker-images/tagma.nix) and here in the dev compose, so the
  # two cannot drift.
  shared = import ../../nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    cacert
    aifed
    binPath
    skillsSeed
    ;

  # Dev topology note: a Caddy edge proxy (services.caddy) is the one
  # entry point -- always on, routing the same `*.<devDomain>` vhosts
  # (default domain kallipai.com; .env sets kallipai.lan) to vite (on
  # the host) / archeion / lesche / files / instances. The edge speaks
  # https with an mkcert certificate by default, or plain http when
  # KALLIP_EDGE_TLS=off; KALLIP_EDGE_PORT moves its listen port (a
  # deployed system caddy keeps the host's real 443). The session
  # cookie carries `Domain=<devDomain>` (see the archeion service env)
  # so it is shared across the sibling subdomains (app. and api.). archeion and lesche
  # still publish 7100/7200 for host-side tooling (kallip-admin, curl);
  # AND for the dev tagma (compose/dev/tagma.nix, host network), which rides the edge's
  # loopback plaintext face (KALLIP_POLIS_URL=http://127.0.0.1:7443) instead.
  # files publishes 7400 and instances 7300 on the loopback interface
  # for the same host-side tooling; the browser path for both rides
  # the edge.

  # The edge owns the entry: caddy is always on and always routes the
  # same subdomain vhosts. KALLIP_EDGE_TLS decides whether the edge
  # speaks https (mkcert leaf cert) or plain http; KALLIP_EDGE_PORT is
  # its listen port (default 443; .env overrides it so a deployed
  # system caddy can keep the host's real 443).
  edgeTls = envOrDefault "KALLIP_EDGE_TLS" "on" == "on";
  edgePort = envOrDefault "KALLIP_EDGE_PORT" "443";
  # The dev domain (registrable domain + subdomain parent). The code
  # default is the prod domain (kallipai.com); .env overrides
  # (kallipai.lan for the dev shape) -- direnv's dotenv puts .env in
  # the shell, so this builtins.getEnv sees it at eval time. Everything
  # below (WebAuthn RP id/origin, CORS, cookie domain, Caddyfile, the
  # web app's API URLs) derives from these bindings.
  devDomain =
    let
      v = builtins.getEnv "KALLIP_DOMAIN";
    in
    if v == "" then "kallipai.com" else v;
  # The browser-facing app origin: the edge's app.<devDomain> face.
  # The port rides along except on its protocol's default (443 for
  # https, 80 for http), matching what the browser's address bar
  # shows -- CORS and OAuth origins compare verbatim.
  appOrigin =
    let
      scheme = if edgeTls then "https" else "http";
      defaultPort = if edgeTls then "443" else "80";
      port = if edgePort == defaultPort then "" else ":${edgePort}";
    in
    "${scheme}://app.${devDomain}${port}";
  # The site-address scheme the Caddyfile sees: empty for the https
  # edge (the tls snippet carries the certificate), http:// for the
  # plain edge (keeps caddy's auto-https out of a non-public TLD).
  edgeScheme = if edgeTls then "" else "http://";
  # The https edge imports its mkcert pair; the plain edge imports an
  # empty snippet, and the http:// site scheme keeps caddy's
  # auto-https out of a non-public TLD.
  edgeTlsSnippet = pkgs.writeText "kallip-edge-tls.caddy" (
    if edgeTls then "tls /certs/cert.pem /certs/key.pem" else ""
  );
  # An IPv4 literal host cannot back a WebAuthn RP id (the builder needs
  # a registrable domain), and passkeys are browser-blocked on plain-http
  # LAN anyway -- so the RP trio degrades to the code defaults there
  # (honest: passkeys simply stay unusable, see .env.example).
  isIpHost = builtins.match "[0-9]+\\.[0-9]+\\.[0-9]+\\.[0-9]+" devDomain != null;

  # Path to the mkcert leaf cert dir (cert.pem + key.pem). Defaults to
  # <repo>/compose/dev/.certs -- where the mkcert command in docs/development.md
  # writes -- so no env var is needed for the common case; override
  # KALLIP_ARION_CERT_PATH only to point elsewhere (e.g. a shared dir across
  # worktrees). If the dir is missing, Caddy fails at runtime with a clear "cert
  # not found" -- the mkcert step in docs/development.md is the prerequisite.
  certDir =
    let
      v = builtins.getEnv "KALLIP_ARION_CERT_PATH";
    in
    if v == "" then
      "${toString ../..}/compose/dev/.certs"
    else if !(lib.hasPrefix "/" v) || lib.hasInfix ":" v then
      throw "arion: KALLIP_ARION_CERT_PATH must be an absolute, colon-free path (got '${v}')"
    else
      v;

  # Bind helper (mirrors compose/dev/tagma.nix): an absolute, colon-free
  # host path -> "<path>:<target>" bind-mount. The named env var wins; unset
  # falls back to the daemon's own default for that mount. Both paths go
  # through the same shape check, so a malformed fallback or override
  # fails fast at eval time instead of half-working at up time.
  bindOverride =
    name: target: fallback:
    let
      v = builtins.getEnv name;
      src = if v == "" then fallback else v;
    in
    if src == "/" || !(lib.hasPrefix "/" src) || lib.hasInfix ":" src then
      throw "arion: ${name} must resolve to an absolute, colon-free host path other than '/' (got '${src}')"
    else
      "${src}:${target}";
  # Unset defaults: the HOST daemon's own code defaults, so a plain arion
  # up manages the SAME real daemon + instance tree a host-side kallipctl
  # sees. The socket bind carries the daemon's runtime-leg socket dir
  # ($XDG_RUNTIME_DIR/kallipai/daemon -- where a desktop daemon binds by
  # default; resolution chain in
  # crates/daemon/kallip-daemon-common/src/socket.rs), the data bind the
  # instance-tree root (~/.local/share/kallipai/tagmata).
  runtimeDir = builtins.getEnv "XDG_RUNTIME_DIR";
  homeDir = builtins.getEnv "HOME";
  instancesStateBind = bindOverride "KALLIP_ARION_INSTANCES_STATE_PATH" "/state" (
    if runtimeDir == "" then
      throw "arion: XDG_RUNTIME_DIR unset; defaulting the instances socket bind needs it (or set KALLIP_ARION_INSTANCES_STATE_PATH)"
    else
      "${runtimeDir}/kallipai/daemon"
  );
  instancesDataBind = bindOverride "KALLIP_ARION_INSTANCES_DATA_PATH" "/data" (
    if homeDir == "" then
      throw "arion: HOME unset; defaulting the instances data bind needs it (or set KALLIP_ARION_INSTANCES_DATA_PATH)"
    else
      "${homeDir}/.local/share/kallipai/tagmata"
  );

  # Parallel-stack overrides (the same env pattern as bindOverride above):
  # unset -> the default single stack; set -> a second, independent dev stack
  # with its own project name (own containers, networks, and volumes), for the
  # dual-archeion acceptance flow:
  #   KALLIP_ARION_PROJECT_NAME=kallipai-dev2 \
  #   KALLIP_ARION_ARCHEION_PORT=7101 KALLIP_ARION_LESCHE_PORT=7201 \
  #   arion up -d archeion lesche
  # Only the HOST side of each publish is overridden (container ports and the
  # compose-internal URLs stay fixed), and caddy keeps routing the
  # archeion2./lesche2. subdomains to the host ports -- so the second stack is
  # reachable exactly where the old inline pair was. Bring up only archeion +
  # lesche (plus their deps): caddy would fight the first stack for :80/:443,
  # and instances owns the loopback-only 7300 (files publishes on all
  # host interfaces since the files page).
  envOrDefault =
    name: default:
    let
      v = builtins.getEnv name;
    in
    if v == "" then default else v;
  projectName = envOrDefault "KALLIP_ARION_PROJECT_NAME" "kallipai-dev";
  archeionHostPort = envOrDefault "KALLIP_ARION_ARCHEION_PORT" "7100";
  lescheHostPort = envOrDefault "KALLIP_ARION_LESCHE_PORT" "7200";
  instancesHostPort = envOrDefault "KALLIP_ARION_INSTANCES_PORT" "7300";
in
{
  imports = [ ./files.nix ];

  config = {
    project.name = projectName;

    # The files service's CORS gate: the browser (app origin) talks to
    # api.<devDomain> cross-origin for uploads/downloads; allowlist the
    # same app origin the archeion uses. Set here (not in files.nix) so
    # appOrigin stays defined in one place; the extra `.service` hop
    # merges with the compose-style env the files module declares.
    services.files.service.environment.KALLIP_FILES_CORS_ORIGINS = appOrigin;
    # Named volumes must be declared at the compose top level (compose rejects
    # a reference to an undeclared named volume). The project name
    # (`kallipai-dev` by default) prefixes every volume, so the internal
    # name only carries the meaningful suffix.
    docker-compose.volumes = {
      archeion_pgdata = { };
      lesche_pgdata = { };
      # The archeion-provisioned internal secret: written by the archeion
      # (rw), read by the lesche, files, and instances (ro).
      polis_internal = { };
    };

    # Dev-only hardcoded creds (prod reads them from .env).
    services.archeion-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "archeion_pgdata:/var/lib/postgresql/data" ];
      service.environment = {
        POSTGRES_USER = "kallip";
        POSTGRES_PASSWORD = "kallip";
        POSTGRES_DB = "kallip";
      };
    };

    services.lesche-postgres = {
      service.image = "postgres:17.5";
      service.volumes = [ "lesche_pgdata:/var/lib/postgresql/data" ];
      service.environment = {
        POSTGRES_USER = "kallip";
        POSTGRES_PASSWORD = "kallip";
        POSTGRES_DB = "kallip";
      };
    };

    # Caddy edge proxy: the one entry point, always on, routing the same
    # subdomain vhosts in both shapes. https (default) terminates TLS
    # with the mkcert leaf cert; plain http drops the cert mount and
    # serves the same routes scheme-less. Host-routes the subdomains to
    # 127.0.0.1: app.<devDomain> -> the host vite dev server (:5173);
    # archeion/lesche -> their host-published ports (:7100/:7200).
    # Runs on the host network (`network_mode: host`) so it can reach the
    # host's vite directly -- under rootless docker the bridge cannot reach
    # host services (host-gateway resolves to a non-routable IP and the host
    # firewall drops the LAN IP). With the host netns, caddy binds the
    # edge port straight on the host (requires
    # net.ipv4.ip_unprivileged_port_start<=80 for port 80 under rootless),
    # so no `ports:` mapping (ignored under host net anyway)
    # and no extra_hosts. The Caddyfile (mounted below) uses
    # {$KALLIP_DOMAIN} / {$KALLIP_EDGE_PORT} substitution; see it for the
    # routing + the streaming flush on lesche.

    # Caddyfile substitutions live on service.environment below.
    services.caddy = {
      service.image = "caddy:2.8";
      service.depends_on = [
        "archeion"
        "lesche"
        "files"
      ];
      service.network_mode = "host";
      service.volumes = [
        "${./Caddyfile.dev}:/etc/caddy/Caddyfile:ro"
        "${edgeTlsSnippet}:/etc/caddy/tls.caddy:ro"
      ]
      ++ lib.optionals edgeTls [
        "${certDir}:/certs:ro"
      ];
      # The substitutions the Caddyfile reads, all sourced from the same
      # nix bindings as the archeion/lesche env below so the whole stack
      # agrees on one name, scheme, and port.
      service.environment = {
        KALLIP_DOMAIN = devDomain;
        KALLIP_EDGE_PORT = edgePort;
        KALLIP_EDGE_SCHEME = edgeScheme;
      };
      service.command = [
        "caddy"
        "run"
        "--config"
        "/etc/caddy/Caddyfile"
      ];
    };

    # Archeion: run from the workspace via the host store; publish 7100 for
    # host-side tooling (kallip-admin, curl). The browser reaches it via Caddy
    # at https://archeion.<devDomain>. dev WebAuthn / CORS / cookie values all
    # derive from the `devDomain` nix binding. (prod-archeion is its own
    # composition: compose/prod/polis.nix, behind the operator's TLS
    # reverse proxy, no published port.)
    services.archeion = {
      service.depends_on = [ "archeion-postgres" ];
      service.useHostStore = true;
      service.command = [ "${workspace}/bin/kallip-archeion" ];
      service.ports = [ "${archeionHostPort}:7100" ];
      # The dev admin token is the pin form (stable value, set here); the
      # generated form writes a fresh token to runtime state on every start
      # and never reaches the logs.
      service.env_file = [ ".env" ];
      service.volumes = [ "polis_internal:/var/lib/kallipai/internal" ];
      # cacert: the reqwest oauth client (rustls) loads the system trust
      # store at startup.
      image.contents = [
        workspace
      ]
      ++ cacert;
      service.environment = {
        PATH = "${workspace}/bin";
        KALLIP_ARCHEION_ADDR = "0.0.0.0:7100";
        KALLIP_ARCHEION_DATABASE_URL = "postgres://kallip:kallip@archeion-postgres:5432/kallip";
        # WebAuthn RP: the id is the registrable domain <devDomain> (an
        # IPv4 literal host degrades to the code-default prod pair --
        # the builder rejects IP RP ids). The origin is appOrigin (the
        # edge's app face, port and all when non-default; ALLOW_ANY_PORT
        # stays false).
        KALLIP_ARCHEION_WEBAUTHN_RP_ID = if isIpHost then "kallipai.com" else devDomain;
        KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN = if isIpHost then "https://app.kallipai.com" else appOrigin;
        KALLIP_ARCHEION_WEBAUTHN_RP_NAME = "kallipai";
        KALLIP_ARCHEION_WEBAUTHN_ALLOW_ANY_PORT = "false";
        # The session cookie is Secure behind the https edge; the plain
        # http edge needs it off.
        KALLIP_ARCHEION_COOKIE_SECURE = if edgeTls then "true" else "false";
        KALLIP_ARCHEION_CORS_ORIGINS = appOrigin;
        # Share the session cookie across app.<devDomain> and api.<devDomain>: the
        # edge serves the app face and the path-routed api face on sibling
        # subdomains in both shapes, so the Domain attribute is
        # what stitches the session across them.
        KALLIP_ARCHEION_SESSION_COOKIE_DOMAIN = devDomain;
        # Caddy runs on the host network and proxies to archeion at 127.0.0.1,
        # so trust loopback for X-Forwarded-For. archeion binds 0.0.0.0:7100
        # (non-loopback), so the boot guard would otherwise clear the trusted
        # set and log every client as 127.0.0.1 (collapsing per-client rate
        # limiting).
        KALLIP_ARCHEION_TRUSTED_PROXIES = "127.0.0.0/8, ::1/128";
        # The platform-internal secret is dev-self-managed, matching prod:
        # first boot generates it into the shared volume, later boots read
        # the existing value; the lesche, files, and instances read the same
        # file (mounted read-only below).
        KALLIP_ARCHEION_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        # The local-platform login refuses to boot with an operator-set
        # admin token shorter than 32 chars, so the dev fixture pins a
        # compliant one HERE (service.environment overrides the shorter
        # KALLIP_ARCHEION_ADMIN_TOKEN a legacy .env may still carry).
        KALLIP_ARCHEION_ADMIN_TOKEN = "sk-admin-dev-0123456789abcdef0123456789abcdef";
        # Local-platform operator login: exchange the admin token for a User
        # session (POST /v1/auth/admin-login) on a fixed local account. Dev
        # fixture, paired with the compliant token above; prod leaves it off.
        KALLIP_ARCHEION_ADMIN_USER_LOGIN = "true";
        RUST_LOG = "info";
        # OAuth callback URLs build from the browser-facing app origin;
        # explicit so the value matches CORS verbatim.
        KALLIP_ARCHEION_OAUTH_REDIRECT_BASE = appOrigin;
      };
    };

    # Lesche: the data-plane relay. Owns the chat domain in its own Postgres
    # (rooms, membership, message payloads) and
    # authenticates + attests identity through the archeion's /internal surface
    # over the compose network. Reached by the browser via Caddy at
    # https://lesche.<devDomain> and by the tagma's relay connector via
    # compose DNS (lesche:7200).
    services.lesche = {
      # No healthcheck; unlike the archeion, lesche does not retry its DB connect.
      service.depends_on = [
        "archeion"
        "lesche-postgres"
      ];
      service.useHostStore = true;
      service.command = [ "${workspace}/bin/kallip-lesche" ];
      service.ports = [ "${lescheHostPort}:7200" ];
      service.env_file = [ ".env" ];
      service.volumes = [ "polis_internal:/var/lib/kallipai/internal:ro" ];
      # reqwest (HttpControlPlane -> archeion /internal) builds its Client at
      # startup and the rustls platform verifier loads the system trust store
      # EAGERLY at .build() -- so the lesche needs the CA bundle at the
      # standard paths (the shared `cacert` wrapper) even though its /internal
      # calls are plain HTTP. Same reason the tagma service carries the CA
      # layer (its in-process relay connector builds a reqwest client at
      # startup).
      image.contents = [
        workspace
      ]
      ++ cacert;
      service.environment = {
        KALLIP_LESCHE_ADDR = "0.0.0.0:7200";
        KALLIP_LESCHE_DATABASE_URL = "postgres://kallip:kallip@lesche-postgres:5432/kallip";
        KALLIP_LESCHE_ARCHEION_INTERNAL_URL = "http://archeion:7100";
        # Read the archeion-provisioned internal secret (shared volume).
        KALLIP_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        KALLIP_LESCHE_INTERNAL_TOKEN = "dev-notify-secret";
        # Allow the web app origin (https://app.<devDomain> via Caddy) to
        # make credentialed cross-origin calls to api.<devDomain>.
        KALLIP_LESCHE_CORS_ORIGINS = appOrigin;
        RUST_LOG = "info";
      };
    };

    # Instances: the local instance management service (kallip-instances),
    # proxying the HOST daemon's UDS socket. Both the socket dir and the
    # instance tree are host bind mounts: the KALLIP_ARION_INSTANCES_*
    # overrides, or unset the HOST daemon's standard dirs (the same real
    # daemon a host-side kallipctl sees). The browser reaches it via
    # https://instances.<devDomain>; host tooling uses the published
    # 127.0.0.1:7300. Platform mode: the archeion's internal root + shared
    # secret verify the SPA's sk-admin- bearer (the local-platform
    # login key), replacing the standalone token. API-only: the SPA is
    # served by the host vite dev server (Caddy @app -> :5173); the
    # browser calls this service cross-origin from the app origin
    # (KALLIP_INSTANCES_CORS_ORIGINS below).
    services.instances = {
      service.useHostStore = true;
      service.command = [ "${workspace}/bin/kallip-instances" ];
      # Loopback-tight publish: browsers ride the edge's
      # instances.<devDomain> vhost in both shapes; the loopback publish
      # serves host-side tooling (kallipctl, curl) only.
      service.ports = [ "127.0.0.1:${instancesHostPort}:7300" ];
      service.env_file = [ ".env" ];
      # The archeion provisions the internal secret this service reads.
      service.depends_on = [ "archeion" ];
      service.volumes = [
        instancesStateBind
        instancesDataBind
        "polis_internal:/var/lib/kallipai/internal:ro"
      ];
      image.contents = [
        workspace
      ]
      ++ cacert;
      service.environment = {
        PATH = "${workspace}/bin";
        KALLIP_INSTANCES_ADDR = "0.0.0.0:7300";
        # The mounted host dir carries the daemon's socket; it points
        # INTO the container mount, never at a host path.
        KALLIP_DAEMON_SOCKET = "/state/control.sock";
        # Platform mode: the archeion's internal face verifies the SPA's
        # sk-admin- bearer; the secret is the archeion-provisioned
        # internal token (shared volume, read-only here).
        KALLIP_INSTANCES_ARCHEION_URL = "http://archeion:7100";
        KALLIP_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/internal/internal-token";
        KALLIP_INSTANCES_ALLOWED_HOSTS = "api.${devDomain}";
        KALLIP_INSTANCES_CORS_ORIGINS = appOrigin;
        RUST_LOG = "info";
      };
    };
  };
}
