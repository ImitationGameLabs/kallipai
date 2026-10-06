# NixOS module for the kallipai platform-hosting form: the daemon
# as a system service, the declared tagma users whose instances it
# launches, and the polis platform services.
#
# Identity is externalized by design: this module declares the users and
# the daemon only consumes passwd entries — it never creates users or
# touches the uid ledger. The three directories split the daemon's world
# (config under /etc, runtime under /run, record area under /var/lib),
# and linger gives every declared user the standard logind runtime
# directory (/run/user/<uid>), the same semantics a human user gets.
#
# The polis section (services.kallipai.polis) brings up the platform
# services -- archeion, lesche, files, instances, and the model gateway --
# on one switch:
# localhost-only listeners behind the host's reverse proxy, one shared
# PostgreSQL for the stateful services over unix-socket peer auth,
# and one self-managed secret: the platform-internal token is generated
# by the archeion into its own state directory on first boot and only
# read afterwards. Operator token knobs stay paths (pin an admin token,
# or let the unit mint a persistent one); no secret hits the store.

# The platform knobs services.kallipai.domain and services.kallipai.tls
# feed every web-facing default (CORS origins, the session cookie,
# webauthn, the baked runtime config); each derivation is a mkDefault,
# so an explicit option still wins.
{ packages, aifedOverlay }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.kallipai.daemon;
  polisCfg = config.services.kallipai.polis;
  gatewayCfg = config.services.kallipai.polis.gateway;
  webCfg = config.services.kallipai.web;

  # The flake's own build for this host: the package options default to
  # it, so enabling a service needs no package reference; setting an
  # option explicitly pins a specific build.
  hostPackages = packages.${pkgs.stdenv.hostPlatform.system};

  # The aifed build from the flake input, resolved through the input's
  # own overlay: the host's pkgs does not necessarily carry it, so the
  # module extends locally and stays self-contained. flake.lock pins
  # the revision; setting the option explicitly pins a specific build.
  aifedDefault = (pkgs.extend aifedOverlay).aifed;
  inherit (import ./lib.nix) bakeRuntimeConfig;

  # Single source of truth for the polis port defaults: the option
  # defaults and the direct-connect warning both read this one
  # binding, so a default change happens here and nowhere else. The
  # web UI's compiled-in copies of these numbers must change with
  # them.
  defaultPolisPorts = {
    archeion = 7100;
    lesche = 7200;
    files = 7400;
    instances = 7300;
  };

  # Same single-source pattern for the gateway's two faces: the data
  # plane takes the 7x01 slot adjacent to the managed-services 7x00
  # family (the LLM-compatible forwarding plane), the management face
  # holds the 7x00 slot. Mirrors the crate's own defaults; the option
  # defaults and the assertions read this one binding.
  defaultGatewayPorts = {
    data = 7501;
    management = 7500;
  };

  # The daemon's control socket: the daemon unit sets it and the
  # instances proxy reads it back, so the path lives in one binding.
  # Keep in sync with SYSTEM_DAEMON_SOCKET in
  # crates/daemon/kallipai-daemon-common/src/socket.rs (the client probe
  # chain's last-resort leg).
  daemonSocket = "/run/kallipai/daemon.sock";

  # Inject an environment key only when the option carries a value: null
  # means "let the service's own default govern", keeping the code default
  # the single source of truth -- a mirrored default here would drift from
  # it. Bools render via boolToString because Nix's toString gives "1".
  envOpt =
    name: value:
    lib.optionalAttrs (value != null) {
      ${name} = if lib.isBool value then lib.boolToString value else toString value;
    };
  # The polis listeners' localhost ports, configured per service under
  # services.kallipai.polis.ports and shared by the env and the edge
  polisPorts = polisCfg.ports;
  # The platform's web-facing knobs and the origins derived from them.
  # Only forced under the domain-is-set guard in the derivation arm
  # (Nix laziness), so a null domain never reaches the interpolation.
  platformDomain = config.services.kallipai.domain;
  platformTls = config.services.kallipai.tls;
  appScheme = if platformTls then "https" else "http";
  appOrigin = "${appScheme}://app.${platformDomain}";
in
{
  options.services.kallipai = {
    domain = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Deployment domain; the web app lives at app.<domain> and the API edge at api.<domain>; every web-facing default derives from it.";
    };

    tls = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Serve the platform over https; switching it off derives http origins and forces the session cookie non-Secure.";
    };

    aifedPackage = lib.mkOption {
      type = lib.types.package;
      default = aifedDefault;
      description = ''
        The aifed editor-face tool: agents shell out to it for file
        edits, and people use it interactively, so it lands on the
        system PATH next to the platform commands. Defaults to the
        aifed flake input's own build (resolved through that input's
        overlay, pinned by flake.lock); set this option to pin a
        specific build.
      '';
    };

    daemon = {
      enable = lib.mkEnableOption "the kallipai daemon as a system service";

      package = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.workspace;
        description = ''
          The kallipai daemon package, defaulting to this flake's full
          workspace build. The unit pins KALLIPAI_BIN_DIR to this
          package's bin directory, so the daemon, its spawn helper,
          and the tagma it launches all come from one build; a
          custom package moves the whole set together.
        '';
      };

      delegates = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = ''
          System accounts allowed to act on behalf of other declared
          users across the instance-management verbs (delegated
          administration). The daemon resolves each name through
          passwd at startup and refuses to start on an unknown name,
          root, or its own account. Empty by default: no delegation.
          Enabling the polis platform services adds the
          kallipai-instances service account automatically.
        '';
      };

      group = lib.mkOption {
        type = lib.types.str;
        default = "kallipai-polis";
        description = ''
          The group gating @<group> access to the nix daemon and read
          access to the archeion's provisioned internal-token file: one
          declaration primitive shared by every platform-internal access
          path. The daemon's control socket is gated separately, by the
          dedicated kallipai-daemon group, so socket admission does
          not ride the platform gate. Deliberately not the generic
          `users` group -- that would hand access to every account on
          the host.
        '';
      };

      polisUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Platform edge origin (e.g. "https://api.example.com") the daemon
          fills into relay-intent spawns that omit it (KALLIPAI_POLIS_URL).
          With polis enabled and a domain set this derives by
          default from services.kallipai.domain
          (<scheme>://api.<domain>, the scheme following
          services.kallipai.tls); an explicit value always
          wins. Set it explicitly on split deployments where the daemon
          host cannot reach api.<domain>. With polis disabled nothing is
          derived: null injects nothing, and a spawn carrying an
          enrollment code but no origin then fails loudly at boot
          (credentials are never sent to an assumed deployment). The
          api.<domain> form beats a service port -- tagma clients append
          /v1/<service>, which only the edge routes.
        '';
      };

      tagmaUsers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = ''
          Pre-declared system users that tagma instances may run as (the
          dedicated-user form of `kallipctl spawn --user`). Each gets a
          home directory and linger, so a spawned instance finds the
          standard XDG runtime directory and can drive flake + direnv
          natively.
        '';
      };
    };
    polis = {
      enable = lib.mkEnableOption "the polis platform services (archeion, lesche, files, instances, and the model gateway) as system services";

      archeionPackage = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.workspace;
        description = "The kallipai-archeion package; defaults to the full workspace build.";
      };
      leschePackage = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.workspace;
        description = "The kallipai-lesche package; defaults to the full workspace build.";
      };
      filesPackage = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.workspace;
        description = "The kallipai-files package; defaults to the full workspace build.";
      };
      instancesPackage = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.workspace;
        description = "The kallipai-instances package; defaults to the full workspace build.";
      };

      adminTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          Path to a root-only EnvironmentFile defining
          KALLIPAI_ARCHEION_ADMIN_TOKEN (the provisioning authority and the
          admin-login exchange), plus any archeion-only extra keys --
          notably the OAuth client secrets
          (KALLIPAI_ARCHEION_OAUTH_GITHUB_CLIENT_SECRET,
          KALLIPAI_ARCHEION_OAUTH_GOOGLE_CLIENT_SECRET; a provider enables
          only when its id option and secret are both set).
          This is the pin form: a stable credential the operator owns, so
          it lives under /etc like other admin assets. Default null means
          the archeion mints the token itself into its state directory
          (/var/lib/kallipai/archeion/admin-token.env, 0600) on first
          boot and reads it (never rewrites) after; rotate with
          kallipai-admin admin-token reset (authenticates with the current
          token). The plaintext never reaches the journal. Pin the file
          to keep a stable token; leave it unset to mint one.
        '';
      };
      notifyTokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          Path to a root-only EnvironmentFile carrying the files-to-lesche
          event-push secret; the file must define two keys with the same value:
          KALLIPAI_LESCHE_INTERNAL_TOKEN and KALLIPAI_FILES_NOTIFY_TOKEN. Default
          null: the lesche leaves its internal surface unmounted and the push
          stays disabled (the safe standalone posture).
        '';
      };

      ports = {
        archeion = lib.mkOption {
          type = lib.types.port;
          default = defaultPolisPorts.archeion;
          description = ''
            Listening port of the archeion service. Must be 1024-65535;
            pick a port outside the system's ephemeral range and unused by
            other services on this host — a conflict surfaces at service
            start as an address-in-use error.
          '';
        };
        lesche = lib.mkOption {
          type = lib.types.port;
          default = defaultPolisPorts.lesche;
          description = ''
            Listening port of the lesche service. Must be 1024-65535;
            pick a port outside the system's ephemeral range and unused by
            other services on this host — a conflict surfaces at service
            start as an address-in-use error.
          '';
        };
        files = lib.mkOption {
          type = lib.types.port;
          default = defaultPolisPorts.files;
          description = ''
            Listening port of the files service. Must be 1024-65535;
            pick a port outside the system's ephemeral range and unused by
            other services on this host — a conflict surfaces at service
            start as an address-in-use error.
          '';
        };
        instances = lib.mkOption {
          type = lib.types.port;
          default = defaultPolisPorts.instances;
          description = ''
            Listening port of the instances service. Must be 1024-65535;
            pick a port outside the system's ephemeral range and unused by
            other services on this host — a conflict surfaces at service
            start as an address-in-use error.
          '';
        };
      };

      archeion = {
        webauthnRpId = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "WebAuthn relying-party id (the registrable domain passkeys bind to); changing it invalidates every bound passkey.";
        };
        webauthnRpOrigin = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "WebAuthn relying-party origin; must have the rp id as its effective domain.";
        };
        webauthnRpName = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Human-readable WebAuthn relying-party name shown in the browser prompt.";
        };
        webauthnAllowAnyPort = lib.mkOption {
          type = lib.types.nullOr lib.types.bool;
          default = null;
          description = "Allow non-standard ports on the WebAuthn origin (local HTTP dev only).";
        };
        sessionTtlSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Session cookie lifetime in seconds.";
        };
        cookieSecure = lib.mkOption {
          type = lib.types.nullOr lib.types.bool;
          default = null;
          description = "Mark the session cookie Secure (disable only for plain-HTTP dev).";
        };
        cookieDomain = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Cookie Domain attribute; set to the parent domain when the lesche shares a subdomain of it.";
        };
        authRateCapacity = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Per-IP token-bucket capacity guarding /v1/archeion/auth/*.";
        };
        authRateRefillPerSec = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Per-IP auth bucket refill rate, requests per second.";
        };
        pairRateCapacity = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Shared pairing-bucket capacity: the real brute-force bound on the pairing code (per-IP limiting is bypassable by source-IP diversity).";
        };
        pairRateRefillPerSec = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Shared pairing-bucket refill rate, requests per second.";
        };
        trustedProxies = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CIDRs trusted to set X-Forwarded-For; the code default already trusts loopback for the same-box proxy.";
        };
        maxBodySizeKb = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Max HTTP request body size in kilobytes (0 = axum default).";
        };
        corsOrigins = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CORS allow-list origins; never a wildcard on a public deploy.";
        };
        enrollmentCodeTtlSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Single-use enrollment-code lifetime in seconds.";
        };
        signupEnabled = lib.mkOption {
          type = lib.types.nullOr lib.types.bool;
          default = null;
          description = "Whether open signup is allowed (the incident-time kill switch).";
        };
        oauthRedirectBase = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Web origin the OAuth flow redirects into; required only when an OAuth provider is configured.";
        };
        oauthGithubClientId = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "GitHub OAuth client id; the provider enables only when id and secret are both present. Client secrets go in the token files, never here.";
        };
        oauthGoogleClientId = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Google OAuth client id; same enable rule and secret rule as GitHub.";
        };
        adminUserLogin = lib.mkOption {
          type = lib.types.nullOr lib.types.bool;
          default = null;
          description = "Mount POST /v1/archeion/auth/admin-login (admin token exchanged for a local session). When on, a hand-set admin token shorter than 32 chars fails at boot.";
        };
      };

      lesche = {
        proofSkewSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.int;
          default = null;
          description = "Acceptable clock skew (both directions) on a tunnel reconnect proof, in seconds.";
        };
        keyExchangeTimeoutSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "How long a synchronous key exchange waits for the tagma before failing with 504.";
        };
        maxBodySizeKb = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Max HTTP request body size in kilobytes (0 = axum default).";
        };
        corsOrigins = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CORS allow-list origins.";
        };
      };

      files = {
        maxBodySizeMb = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Maximum accepted upload body in megabytes; larger streams get 413.";
        };
        corsOrigins = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CORS allow-list origins.";
        };
        degrade = lib.mkOption {
          type = lib.types.nullOr (
            lib.types.enum [
              "closed"
              "soft"
            ]
          );
          default = null;
          description = "Archeion degrade posture: closed fails authorization with 503 when the registry is unreachable, soft denies with 403 from an empty fact set.";
        };
        gcIntervalSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Delay between GC passes, in seconds.";
        };
        gcGraceSecs = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "How long a freed zero-refcount row must age before the GC reclaims it.";
        };
        gcBatch = lib.mkOption {
          type = lib.types.nullOr lib.types.ints.unsigned;
          default = null;
          description = "Maximum catalog rows reclaimed per GC pass.";
        };
      };
      instances = {
        corsOrigins = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CORS allow-list origins; the web bundle calls this service cross-origin (app.<domain> against the api.<domain> edge), so a proxied deployment serving the web app lists that origin here.";
        };
        allowedHosts = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated extra Host values the host guard admits (IP literals and localhost always pass). The proxied shape receives api.<domain>; a direct-connect LAN shape names the domain browsers use.";
        };
      };
      gateway = {
        enable = lib.mkEnableOption "the kallipai model gateway as a system service" // {
          default = true;
        };

        package = lib.mkOption {
          type = lib.types.package;
          default = hostPackages.workspace;
          description = "The kallipai-model-gateway package; defaults to the full workspace build.";
        };

        port = lib.mkOption {
          type = lib.types.port;
          default = defaultGatewayPorts.data;
          description = ''
            Listening port of the gateway's data plane (distribution reads,
            forwarding, health). Must be 1024-65535; a conflict surfaces at
            service start as an address-in-use error.
          '';
        };

        managementPort = lib.mkOption {
          type = lib.types.port;
          default = defaultGatewayPorts.management;
          description = ''
            Listening port of the management plane (the /admin family).
            Binds the loopback and is not published by the module: the admin
            namespace is house-only.
          '';
        };

        corsOrigins = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = "Comma-separated CORS allow-list origins; never a wildcard on a public deploy.";
        };

        publicBaseUrl = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = ''
            The gateway's own base URL as clients should reach it (the
            base_url field of every distributed profile; consumers append
            their wire path to it). Must include the /v1 prefix. The /v1
            suffix is a client-side convention: the edge strips it before
            the gateway (the server-side routes carry no /v1 prefix).
            Use the model-gw host form, never the api-edge segment: the
            api edge routes /v1/model-gateway/* to the gateway's
            management face, so a forwarding base on that segment
            reaches no forwarding route.
            Default null derives https://model-gw.<domain>/v1 when the
            platform domain is set; with no domain the crate's loopback
            default stands.
          '';
        };

      };
    };
    web = {
      enable = lib.mkEnableOption "the kallipai-web site root artifact (the bundle, or the bundle with the runtime config baked in)";

      package = lib.mkOption {
        type = lib.types.package;
        default = hostPackages.kallipai-web-dist;
        description = ''
          The kallipai-web bundle (this flake's kallipai-web-dist build);
          set it explicitly to pin a specific build, as with the polis
          packages.
        '';
      };

      runtimeConfig = lib.mkOption {
        type = lib.types.submodule {
          options = {
            domain = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Deployment domain (the app face is app.<domain>); defaults to services.kallipai.domain when that is set.";
            };
            offlineLogin = lib.mkOption {
              type = lib.types.nullOr lib.types.bool;
              default = null;
              description = "True = show the operator-key login branch.";
            };
            apiBase = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Platform-edge-origin override (e.g. \"https://api.example.com\"); every service base becomes <apiBase>/v1/<service>; an empty string counts as unset.";
            };
            gatewayAdminUrl = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = ''
                The model gateway admin base URL the admin page prefills
                (the /admin face behind its own token); an empty string
                counts as unset and the page asks the operator for the URL.
              '';
            };
          };
        };
        default = { };
        description = ''
          Values for the web app's runtime config (/config.js), baked
          into the site root as a window.KALLIPAI_CONFIG assignment. Keys
          mirror the app's Window.KALLIPAI_CONFIG type: domain,
          gatewayAdminUrl, offlineLogin, apiBase; an unknown key fails
          evaluation. The
          empty default keeps the shipped config.js defaults. Set
          offlineLogin = false to hide the operator-key login branch (a
          cloud-facing deployment), or add domain/apiBase to pin values
          the app would otherwise derive from the browser location;
          domain already defaults to services.kallipai.domain. The file
          is baked into the site root and served publicly, so anything
          placed here is readable by anyone who can reach the site —
          keep it to values the browser is meant to see; secrets belong
          in environment files or credential stores, never in this
          option.
        '';
      };

      distWithRuntimeConfig = lib.mkOption {
        type = lib.types.package;
        description = ''
          The site root an edge server serves: the bundle as-is when
          runtimeConfig is empty (the shipped config.js stands), or the
          bundle with a config.js baked from runtimeConfig. Derived by
          default; set it to pin a custom-built root. See
          docs/en/deployment/nixos/ for the serving example.
        '';
      };
    };

  };
  config = lib.mkMerge [
    {
      assertions =
        lib.optionals polisCfg.enable (
          # The four listeners must not collide: a shared port is always a
          # misconfiguration, so fail at eval time with the pair and value.
          (map
            (pair: {
              assertion = polisCfg.ports.${builtins.elemAt pair 0} != polisCfg.ports.${builtins.elemAt pair 1};
              message = "services.kallipai.polis.ports.${builtins.elemAt pair 0} and services.kallipai.polis.ports.${builtins.elemAt pair 1} are both ${
                toString polisCfg.ports.${builtins.elemAt pair 0}
              }; the four polis listeners must use distinct ports — set one of them to a free port.";
            })
            [
              [
                "archeion"
                "lesche"
              ]
              [
                "lesche"
                "files"
              ]
              [
                "archeion"
                "files"
              ]
              [
                "archeion"
                "instances"
              ]
              [
                "lesche"
                "instances"
              ]
              [
                "files"
                "instances"
              ]
            ]
          )
          ++ (map
            (svc: {
              assertion = polisCfg.ports.${svc} >= 1024 && polisCfg.ports.${svc} <= 65535;
              message = "services.kallipai.polis.ports.${svc} is ${toString polisCfg.ports.${svc}}; it must be 1024-65535 — the polis services do not hold CAP_NET_BIND_SERVICE, so a lower port cannot be bound. Set it to a port in that range.";
            })
            [
              "archeion"
              "lesche"
              "files"
              "instances"
            ]
          )
        )
        ++ (lib.optionals gatewayCfg.enable [
          {
            assertion = gatewayCfg.port != gatewayCfg.managementPort;
            message = "services.kallipai.polis.gateway.port and services.kallipai.polis.gateway.managementPort are both ${toString gatewayCfg.port}; the gateway's data and management faces must use distinct ports — set one of them to a free port.";
          }
        ])
        ++ (lib.optionals gatewayCfg.enable (
          map
            (face: {
              assertion = gatewayCfg.${face} >= 1024 && gatewayCfg.${face} <= 65535;
              message = "services.kallipai.polis.gateway.${face} is ${toString gatewayCfg.${face}}; it must be 1024-65535 — the gateway does not hold CAP_NET_BIND_SERVICE, so a lower port cannot be bound. Set it to a port in that range.";
            })
            [
              "port"
              "managementPort"
            ]
        ))
        ++ (lib.optionals (polisCfg.enable && gatewayCfg.enable) (
          map
            (pair: {
              assertion = polisCfg.ports.${builtins.elemAt pair 0} != gatewayCfg.${builtins.elemAt pair 1};
              message = "services.kallipai.polis.ports.${builtins.elemAt pair 0} and services.kallipai.polis.gateway.${builtins.elemAt pair 1} are both ${
                toString polisCfg.ports.${builtins.elemAt pair 0}
              }; the polis listeners and the gateway faces must use distinct ports — set one of them to a free port.";
            })
            [
              [
                "archeion"
                "port"
              ]
              [
                "lesche"
                "port"
              ]
              [
                "files"
                "port"
              ]
              [
                "instances"
                "port"
              ]
              [
                "archeion"
                "managementPort"
              ]
              [
                "lesche"
                "managementPort"
              ]
              [
                "files"
                "managementPort"
              ]
              [
                "instances"
                "managementPort"
              ]
            ]
        ));
    }
    (lib.mkIf cfg.enable {
      # One group per declared user plus the shared access gate group
      # (nix @group below, and the polis token gate in the polis
      # block). Primary groups are per user: a shared primary group
      # would let the tagma users read each other's homes, undoing the
      # uid isolation the dedicated-user form exists for. The gate
      # group rides as an extra group — kernel group checks accept
      # supplementary membership, so the nix gate works unchanged. The
      # daemon's control socket is gated separately (kallipai-daemon
      # below): that group admits only actual socket consumers.
      users.groups = lib.listToAttrs (
        map (name: lib.nameValuePair name { }) (
          cfg.tagmaUsers
          ++ [
            cfg.group
            "kallipai-daemon"
          ]
        )
      );

      users.users = lib.listToAttrs (
        map (
          name:
          lib.nameValuePair name {
            isSystemUser = true;
            group = name;
            extraGroups = [ cfg.group ];
            home = "/home/${name}";
            createHome = true;
            # logind pre-creates /run/user/<uid> at boot: the spawned
            # instance's XDG_RUNTIME_DIR, no per-instance setup.
            linger = true;
            shell = pkgs.bashInteractive;
          }
        ) cfg.tagmaUsers
      );

      # The upstream default is [ "*" ] — everyone. Concatenating with a
      # default would defeat the wiring, so this module owns the list:
      # the declared users (by name) and the tagma group. This replaces
      # anything an operator configured elsewhere — extra entries belong
      # in tagmaUsers, not in a competing declaration.
      nix.settings.allowed-users = lib.mkForce (lib.unique (cfg.tagmaUsers ++ [ "@${cfg.group}" ]));

      # Enabling the daemon puts every platform command on PATH (the
      # full workspace build), plus the aifed editor tool that agents
      # shell out to and people use interactively. The services still
      # run from their own packages -- PATH is for people and their
      # agents, not for the systemd units.
      environment.systemPackages = [
        hostPackages.workspace
        config.services.kallipai.aifedPackage
      ];

      # The daemon's unit environment only covers the daemon itself;
      # interactive kallipctl sessions resolve the socket through
      # KALLIPAI_DAEMON_SOCKET first, so expose it session-wide.
      environment.sessionVariables.KALLIPAI_DAEMON_SOCKET = daemonSocket;

      systemd.services.kallipai-daemon = {
        description = "kallipai instance daemon";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];

        # A NixOS unit's PATH is empty unless the unit lists `path`.
        # The daemon's own binaries ride KALLIPAI_BIN_DIR (pinned
        # below); the system path stays for anything else that
        # expects a standard PATH inside the unit.
        path = [ config.system.path ];
        environment = {
          # The spawn helper execs the tagma with execve, which does
          # not search PATH: pin the bin directory of the same package
          # the unit runs, so daemon, helper, and tagma stay one build.
          KALLIPAI_BIN_DIR = "${cfg.package}/bin";
          KALLIPAI_DAEMON_SOCKET = daemonSocket;
          KALLIPAI_DAEMON_RECORD_DIR = "/var/lib/kallipai/daemon/instances";
          # Dedicated socket gate, not the shared platform gate: the group
          # admits only actual socket consumers. The tagma users keep the
          # nix gate (which the platform gate group carries) untouched.
          KALLIPAI_DAEMON_SOCKET_GROUP = "kallipai-daemon";
          # Delegated administration: the polis gate contributes the
          # instances proxy's own account; the daemon refuses an
          # unresolvable name, so option and service drift is loud.
          KALLIPAI_DAEMON_DELEGATES = lib.concatStringsSep "," (
            lib.unique (cfg.delegates ++ lib.optional polisCfg.enable "kallipai-instances")
          );
          # NixOS has no /bin/bash; the login-environment harvest needs a
          # fixed administrative bash, never the caller's shell.
          KALLIPAI_HARVEST_BASH = "${pkgs.bash}/bin/bash";
          # Relay default: the platform edge origin the daemon fills into
          # relay-intent spawns that omit it (before the record snapshot
          # is written, so restarts replay the filled env). null injects
          # nothing -- see the polisUrl option.
        }
        // envOpt "KALLIPAI_POLIS_URL" cfg.polisUrl;

        serviceConfig = {
          ExecStart = "${cfg.package}/bin/kallipai-daemon";
          # Dedicated-user launches fork+setuid to arbitrary declared
          # users: that needs real root, not a capability subset.
          User = "root";
          StateDirectory = "kallipai/daemon";
          # Records enumerate the slugs, uids, and workspaces on the
          # host; 0700 keeps that a root-and-daemon-only view (clients
          # read through the socket, not the files).
          StateDirectoryMode = "0700";
          RuntimeDirectory = "kallipai";
          ConfigurationDirectory = "kallipai";
          Restart = "on-failure";
          # A crash-looping unit must not slam the start-rate limit
          # and lock itself out of restarting (archeion precedent).
          RestartSec = "5s";
          # Tagmata are forked+setsid'd by kallipai-daemon-spawn: they
          # leave the session but stay in this unit's cgroup, so the
          # default control-group kill would sweep every live instance
          # whenever the daemon restarts — an upgrade would kill
          # running work. "process" kills only the daemon; instances
          # keep running and adopt the new binary at their next
          # deliberate kallipctl stop/start, and a daemon crash with
          # Restart = "on-failure" does not cascade either.
          KillMode = "process";
        };
      };
    })
    (lib.mkIf polisCfg.enable {
      # Dedicated users, per-user primary groups. The stateful services
      # stay out of the daemon-socket gate group (kallipai-daemon):
      # none of them consumes the socket. They ride the platform gate
      # group only where the archeion token requires it.
      users.groups = builtins.listToAttrs (
        map (name: lib.nameValuePair name { }) [
          "kallipai-archeion"
          "kallipai-lesche"
          "kallipai-files"
          "kallipai-instances"
          "kallipai-model-gateway"
        ]
      );
      users.users =
        builtins.listToAttrs (
          map
            (
              name:
              lib.nameValuePair name {
                isSystemUser = true;
                group = name;
                # Consumers ride the platform gate group for the archeion's
                # 0640 internal-token file. The daemon socket is gated
                # separately (kallipai-daemon) and none of these users
                # joins it. The archeion user keeps its primary group private;
                # its unit runs with the gate group as primary instead, so
                # membership rides the unit, not this user declaration.
                extraGroups = lib.optionals (name != "kallipai-archeion") [ cfg.group ];
              }
            )
            [
              "kallipai-archeion"
              "kallipai-lesche"
              "kallipai-files"
              "kallipai-model-gateway"
            ]
        )
        // {
          # Explicit for symmetry with the map above; see its comment for
          # the gate-group rationale.
          kallipai-instances = {
            isSystemUser = true;
            group = "kallipai-instances";
            # The one polis user that dials the daemon's control socket:
            # platform gate for the token, socket gate for the proxy.
            extraGroups = [
              cfg.group
              "kallipai-daemon"
            ];
          };
        };

      # One shared PostgreSQL over the unix socket: each service connects as
      # its own system user (peer auth), so no password exists to leak and
      # no TCP surface exists. Database name = role name = OS user name (one
      # name, hyphenated: peer maps the OS user to the role, and
      # ensureDBOwnership runs ALTER DATABASE on the role name). Peer
      # authentication is pinned explicitly so it does not depend on the
      # channel's implicit default.
      services.postgresql = {
        enable = lib.mkDefault true;
        authentication = lib.mkDefault "local all all peer";
        ensureDatabases = lib.mkDefault [
          "kallipai-archeion"
          "kallipai-lesche"
          "kallipai-files"
        ];
        ensureUsers = lib.mkDefault [
          {
            name = "kallipai-archeion";
            ensureDBOwnership = true;
          }
          {
            name = "kallipai-lesche";
            ensureDBOwnership = true;
          }
          {
            name = "kallipai-files";
            ensureDBOwnership = true;
          }
        ];
      };

      systemd.services = {
        kallipai-archeion = {
          description = "kallipai archeion control plane";
          wantedBy = [ "multi-user.target" ];
          # The archeion retries its DB connect with a capped backoff, so
          # wants (not requires): a slow postgres must not tear it down.
          after = [
            "network.target"
            "postgresql.service"
          ];
          wants = [ "postgresql.service" ];
          environment = {
            KALLIPAI_ARCHEION_ADDR = "127.0.0.1:${toString polisPorts.archeion}";
            KALLIPAI_ARCHEION_DATABASE_URL = "postgresql:///kallipai-archeion?host=/run/postgresql";
            KALLIPAI_ARCHEION_LOG_DIR = "/var/log/kallipai/archeion";
            # The internal token is state the archeion owns: generated into
            # its state dir on first boot, read (never rewritten) after.
            KALLIPAI_ARCHEION_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/archeion/internal-token";
            # The minted admin token: generated into the state dir on first
            # boot, read (never rewritten) after; rotate via kallipai-admin
            # admin-token reset.
            KALLIPAI_ARCHEION_ADMIN_TOKEN_OUT_FILE = "/var/lib/kallipai/archeion/admin-token.env";
          }
          // envOpt "KALLIPAI_ARCHEION_WEBAUTHN_RP_ID" polisCfg.archeion.webauthnRpId
          // envOpt "KALLIPAI_ARCHEION_WEBAUTHN_RP_ORIGIN" polisCfg.archeion.webauthnRpOrigin
          // envOpt "KALLIPAI_ARCHEION_WEBAUTHN_RP_NAME" polisCfg.archeion.webauthnRpName
          // envOpt "KALLIPAI_ARCHEION_WEBAUTHN_ALLOW_ANY_PORT" polisCfg.archeion.webauthnAllowAnyPort
          // envOpt "KALLIPAI_ARCHEION_SESSION_TTL_SECS" polisCfg.archeion.sessionTtlSecs
          // envOpt "KALLIPAI_ARCHEION_COOKIE_SECURE" polisCfg.archeion.cookieSecure
          // envOpt "KALLIPAI_ARCHEION_SESSION_COOKIE_DOMAIN" polisCfg.archeion.cookieDomain
          // envOpt "KALLIPAI_ARCHEION_AUTH_RATE_CAPACITY" polisCfg.archeion.authRateCapacity
          // envOpt "KALLIPAI_ARCHEION_AUTH_RATE_REFILL_PER_SEC" polisCfg.archeion.authRateRefillPerSec
          // envOpt "KALLIPAI_ARCHEION_PAIR_RATE_CAPACITY" polisCfg.archeion.pairRateCapacity
          // envOpt "KALLIPAI_ARCHEION_PAIR_RATE_REFILL_PER_SEC" polisCfg.archeion.pairRateRefillPerSec
          // envOpt "KALLIPAI_ARCHEION_TRUSTED_PROXIES" polisCfg.archeion.trustedProxies
          // envOpt "KALLIPAI_ARCHEION_MAX_BODY_SIZE_KB" polisCfg.archeion.maxBodySizeKb
          // envOpt "KALLIPAI_ARCHEION_CORS_ORIGINS" polisCfg.archeion.corsOrigins
          // envOpt "KALLIPAI_ARCHEION_ENROLLMENT_CODE_TTL_SECS" polisCfg.archeion.enrollmentCodeTtlSecs
          // envOpt "KALLIPAI_ARCHEION_SIGNUP_ENABLED" polisCfg.archeion.signupEnabled
          // envOpt "KALLIPAI_ARCHEION_OAUTH_REDIRECT_BASE" polisCfg.archeion.oauthRedirectBase
          // envOpt "KALLIPAI_ARCHEION_OAUTH_GITHUB_CLIENT_ID" polisCfg.archeion.oauthGithubClientId
          // envOpt "KALLIPAI_ARCHEION_OAUTH_GOOGLE_CLIENT_ID" polisCfg.archeion.oauthGoogleClientId
          // envOpt "KALLIPAI_ARCHEION_ADMIN_USER_LOGIN" polisCfg.archeion.adminUserLogin;
          serviceConfig = {
            ExecStart = "${polisCfg.archeionPackage}/bin/kallipai-archeion";
            User = "kallipai-archeion";
            # The unit runs with the gate group as its primary: systemd
            # owns the state tree to it (recursively, every start), so the
            # token is born gate-owned and consumers traverse/read it.
            Group = cfg.group;
            StateDirectory = "kallipai/archeion";
            LogsDirectory = "kallipai/archeion";
            # 0750 (not 0700): gate-group consumers traverse this directory
            # to read the provisioned internal token.
            StateDirectoryMode = "0750";
            LogsDirectoryMode = "0750";
            Restart = "on-failure";
            # A crash-looping archeion must not slam the unit start-rate
            # limit and lock itself out of restarting.
            RestartSec = "5s";
            EnvironmentFile = lib.optional (polisCfg.adminTokenFile != null) (toString polisCfg.adminTokenFile);
          };
        };

        kallipai-lesche = {
          description = "kallipai lesche data-plane relay";
          wantedBy = [ "multi-user.target" ];
          # Soft dependency: the lesche reads the archeion's provisioned
          # internal-token file at boot, so the archeion starts first.
          # If the archeion goes down later, the lesche keeps running
          # degraded (auth answers 503) and recovers on its own -- an
          # archeion crash no longer cascades here.
          after = [
            "network.target"
            "kallipai-archeion.service"
          ];
          wants = [ "kallipai-archeion.service" ];
          environment = {
            KALLIPAI_LESCHE_ADDR = "127.0.0.1:${toString polisPorts.lesche}";
            KALLIPAI_LESCHE_ARCHEION_INTERNAL_URL = "http://127.0.0.1:${toString polisPorts.archeion}";
            KALLIPAI_LESCHE_DATABASE_URL = "postgresql:///kallipai-lesche?host=/run/postgresql";
            KALLIPAI_LESCHE_LOG_DIR = "/var/log/kallipai/lesche";
            KALLIPAI_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/archeion/internal-token";
          }
          // envOpt "KALLIPAI_LESCHE_PROOF_SKEW_SECS" polisCfg.lesche.proofSkewSecs
          // envOpt "KALLIPAI_LESCHE_KEY_EXCHANGE_TIMEOUT_SECS" polisCfg.lesche.keyExchangeTimeoutSecs
          // envOpt "KALLIPAI_LESCHE_MAX_BODY_SIZE_KB" polisCfg.lesche.maxBodySizeKb
          // envOpt "KALLIPAI_LESCHE_CORS_ORIGINS" polisCfg.lesche.corsOrigins;
          serviceConfig = {
            ExecStart = "${polisCfg.leschePackage}/bin/kallipai-lesche";
            User = "kallipai-lesche";
            Group = "kallipai-lesche";
            StateDirectory = "kallipai/lesche";
            LogsDirectory = "kallipai/lesche";
            StateDirectoryMode = "0700";
            LogsDirectoryMode = "0750";
            Restart = "on-failure";
            # A crash-looping unit must not slam the start-rate limit
            # and lock itself out of restarting (archeion precedent).
            RestartSec = "5s";
            EnvironmentFile = lib.optional (polisCfg.notifyTokenFile != null) (
              toString polisCfg.notifyTokenFile
            );
          };
        };

        kallipai-files = {
          description = "kallipai files content-transfer service";
          wantedBy = [ "multi-user.target" ];
          after = [
            "network.target"
            "kallipai-archeion.service"
          ];
          # Soft dependency: the internal-token file is provisioned by
          # the archeion first (see after); if the archeion goes down
          # later, this unit keeps running degraded and recovers on
          # its own.
          wants = [ "kallipai-archeion.service" ];
          environment = {
            KALLIPAI_FILES_ADDR = "127.0.0.1:${toString polisPorts.files}";
            KALLIPAI_FILES_ARCHEION_INTERNAL_URL = "http://127.0.0.1:${toString polisPorts.archeion}";
            KALLIPAI_FILES_DATABASE_URL = "postgresql:///kallipai-files?host=/run/postgresql";
            KALLIPAI_FILES_LOG_DIR = "/var/log/kallipai/files";
            KALLIPAI_FILES_BLOB_ROOT = "/var/lib/kallipai/files/blobs";
            KALLIPAI_FILES_NOTIFY_URL = "http://127.0.0.1:${toString polisPorts.lesche}";
            KALLIPAI_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/archeion/internal-token";
          }
          // envOpt "KALLIPAI_FILES_MAX_BODY_SIZE_MB" polisCfg.files.maxBodySizeMb
          // envOpt "KALLIPAI_FILES_CORS_ORIGINS" polisCfg.files.corsOrigins
          // envOpt "KALLIPAI_FILES_DEGRADE" polisCfg.files.degrade
          // envOpt "KALLIPAI_FILES_GC_INTERVAL_SECS" polisCfg.files.gcIntervalSecs
          // envOpt "KALLIPAI_FILES_GC_GRACE_SECS" polisCfg.files.gcGraceSecs
          // envOpt "KALLIPAI_FILES_GC_BATCH" polisCfg.files.gcBatch;
          serviceConfig = {
            ExecStart = "${polisCfg.filesPackage}/bin/kallipai-files";
            User = "kallipai-files";
            Group = "kallipai-files";
            StateDirectory = "kallipai/files";
            LogsDirectory = "kallipai/files";
            StateDirectoryMode = "0700";
            LogsDirectoryMode = "0750";
            Restart = "on-failure";
            # A crash-looping unit must not slam the start-rate limit
            # and lock itself out of restarting (archeion precedent).
            RestartSec = "5s";
            EnvironmentFile = lib.optional (polisCfg.notifyTokenFile != null) (
              toString polisCfg.notifyTokenFile
            );
          };
        };
        kallipai-instances = {
          description = "kallipai instances management proxy";
          wantedBy = [ "multi-user.target" ];
          # Two dependencies, one posture: both are soft. The daemon
          # answers a restart with 503 daemon_unreachable, not a failed
          # unit; the archeion's provisioned internal-token file is read
          # at boot, so the archeion starts first, and an outage later
          # leaves this unit running degraded (auth answers 503) until
          # it returns.
          after = [
            "network.target"
            "kallipai-daemon.service"
            "kallipai-archeion.service"
          ];
          wants = [
            "kallipai-daemon.service"
            "kallipai-archeion.service"
          ];
          environment = {
            KALLIPAI_INSTANCES_ADDR = "127.0.0.1:${toString polisPorts.instances}";
            KALLIPAI_DAEMON_SOCKET = daemonSocket;
            KALLIPAI_INSTANCES_ARCHEION_URL = "http://127.0.0.1:${toString polisPorts.archeion}";
            KALLIPAI_POLIS_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/archeion/internal-token";
            KALLIPAI_INSTANCES_LOG_DIR = "/var/log/kallipai/instances";
          }
          // envOpt "KALLIPAI_INSTANCES_CORS_ORIGINS" polisCfg.instances.corsOrigins
          // envOpt "KALLIPAI_INSTANCES_ALLOWED_HOSTS" polisCfg.instances.allowedHosts;
          serviceConfig = {
            ExecStart = "${polisCfg.instancesPackage}/bin/kallipai-instances";
            User = "kallipai-instances";
            Group = "kallipai-instances";
            # State stays with the daemon (a pure UDS proxy mounts none);
            # the unit owns only its rolling log directory.
            LogsDirectory = "kallipai/instances";
            LogsDirectoryMode = "0750";
            Restart = "on-failure";
            # A crash-looping unit must not slam the start-rate limit
            # and lock itself out of restarting (archeion precedent).
            RestartSec = "5s";
          };
        };
      };
    })
    # The model gateway: a polis member with the same posture (dedicated
    # user, unix-socket peer auth, localhost-only listeners), but no state
    # directory -- every durable surface (profiles, keys, request audits)
    # lives in PostgreSQL, so the unit owns only its rolling log directory.
    (lib.mkIf (polisCfg.enable && gatewayCfg.enable) {

      services.postgresql = {
        enable = lib.mkDefault true;
        authentication = lib.mkDefault "local all all peer";
        ensureDatabases = lib.mkDefault [ "kallipai-model-gateway" ];
        ensureUsers = lib.mkDefault [
          {
            name = "kallipai-model-gateway";
            ensureDBOwnership = true;
          }
        ];
      };

      systemd.services.kallipai-model-gateway = {
        description = "kallipai model gateway";
        wantedBy = [ "multi-user.target" ];
        # Soft dependency: the internal-token file is provisioned by
        # the archeion first (see after); if the archeion goes down
        # later, this unit keeps running degraded and recovers on
        # its own.
        after = [
          "network.target"
          "postgresql.service"
          "kallipai-archeion.service"
        ];
        wants = [
          "postgresql.service"
          "kallipai-archeion.service"
        ];
        environment = {
          KALLIPAI_MODEL_GATEWAY_ADDR = "127.0.0.1:${toString gatewayCfg.port}";
          KALLIPAI_MODEL_GATEWAY_MANAGEMENT_ADDR = "127.0.0.1:${toString gatewayCfg.managementPort}";
          KALLIPAI_MODEL_GATEWAY_DATABASE_URL = "postgresql:///kallipai-model-gateway?host=/run/postgresql";
          KALLIPAI_MODEL_GATEWAY_LOG_DIR = "/var/log/kallipai/model-gateway";
          KALLIPAI_MODEL_GATEWAY_ARCHEION_URL = "http://127.0.0.1:${toString polisPorts.archeion}";
          KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE = "/var/lib/kallipai/archeion/internal-token";
        }
        // envOpt "KALLIPAI_MODEL_GATEWAY_CORS_ORIGINS" gatewayCfg.corsOrigins
        // envOpt "KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL" gatewayCfg.publicBaseUrl;
        serviceConfig = {
          ExecStart = "${gatewayCfg.package}/bin/kallipai-model-gateway";
          User = "kallipai-model-gateway";
          Group = "kallipai-model-gateway";
          # No local state: the durable surfaces all live in PostgreSQL.
          LogsDirectory = "kallipai/model-gateway";
          LogsDirectoryMode = "0750";
          Restart = "on-failure";
          # A crash-looping unit must not slam the start-rate limit
          # and lock itself out of restarting (archeion precedent).
          RestartSec = "5s";
        };
      };
    })
    (lib.mkIf webCfg.enable {
      # The site root: straight-through when no runtime key is set (the
      # shipped config.js already carries the factory default), the
      # bundle with a baked config.js otherwise. mkDefault keeps a
      # user-set site root in charge.
      services.kallipai.web.distWithRuntimeConfig = lib.mkDefault (
        let
          userKeys = lib.filterAttrs (_: v: v != null) webCfg.runtimeConfig;
        in
        if userKeys == { } then
          webCfg.package
        else
          bakeRuntimeConfig {
            inherit pkgs;
            inherit (webCfg) package;
            runtimeConfig = userKeys;
          }
      );
    })
    # Platform-derived service defaults: every value embeds the domain,
    # so the whole arm waits for one to be set; each mkDefault keeps an
    # operator-set option in charge.
    (lib.mkIf (platformDomain != null) {
      services.kallipai = {
        polis = {
          archeion = {
            corsOrigins = lib.mkDefault appOrigin;
            cookieDomain = lib.mkDefault platformDomain;
            webauthnRpId = lib.mkDefault platformDomain;
            webauthnRpOrigin = lib.mkDefault appOrigin;
            oauthRedirectBase = lib.mkDefault appOrigin;
          };
          lesche.corsOrigins = lib.mkDefault appOrigin;
          files.corsOrigins = lib.mkDefault appOrigin;
          instances = {
            corsOrigins = lib.mkDefault appOrigin;
            allowedHosts = lib.mkDefault "api.${platformDomain}";
          };
          gateway.publicBaseUrl = lib.mkDefault "${appScheme}://model-gw.${platformDomain}/v1";
          # The admin page (the web origin) is the only browser consumer
          # of the gateway faces; the tagma data-plane clients are server
          # side and carry no CORS semantics.
          gateway.corsOrigins = lib.mkDefault appOrigin;
        };
        web.runtimeConfig.domain = lib.mkDefault platformDomain;
        web.runtimeConfig.gatewayAdminUrl = lib.mkDefault (
          let
            apiBase = webCfg.runtimeConfig.apiBase or null;
          in
          if apiBase != null && apiBase != "" then
            "${apiBase}/v1/model-gateway"
          else
            "${appScheme}://api.${platformDomain}/v1/model-gateway"
        );
      };
    })
    # Module default: derive the daemon's relay fill origin from the
    # platform domain. Gated on polis itself -- the option only feeds
    # the daemon unit, and with polis disabled nothing is derived.
    (lib.mkIf (platformDomain != null && polisCfg.enable) {
      services.kallipai.daemon.polisUrl = lib.mkDefault "${appScheme}://api.${platformDomain}";
    })
    # Cookie security tracks the tls knob only downward: https leaves
    # the option null (the code default already sends Secure), plain
    # http forces it off because a Secure cookie would never round-trip.
    {
      services.kallipai.polis.archeion.cookieSecure = lib.mkIf (!platformTls) (lib.mkDefault false);
    }
  ];
}
