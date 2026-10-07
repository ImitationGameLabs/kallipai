{
  pkgs,
  lib,
  aifed,
  project,
}:
{
  # Evaluate the NixOS module (pure eval): a stub host with
  # every switch on must typecheck, pass its assertions, and stay
  # warning-free; the domain knob must drive the derived defaults; and
  # the merged site root must carry the baked runtime config. Builds
  # here: a stub bundle only, seconds on a warm store.
  "${project}-nixos-module-eval" =
    let
      stubPackages = {
        ${pkgs.stdenv.hostPlatform.system} = builtins.listToAttrs (
          map
            (name: {
              inherit name;
              # The web stub ships a config.js shell and a page sentinel:
              # the plain site root must pass both through; a baked root
              # must overwrite the former and carry the latter.
              value = pkgs.runCommand "${name}-stub" { } (
                if name == "kallipai-web-dist" then
                  ''
                    mkdir $out
                    echo bundle-shell > $out/config.js
                    echo bundle-page > $out/index.html
                  ''
                else
                  "mkdir $out"
              );
            })
            [
              "kallipai-daemon"
              "kallipai-archeion"
              "kallipai-lesche"
              "kallipai-files"
              "kallipai-instances"
              "kallipai-model-gateway"
              "kallipai-web-dist"
              "workspace"
            ]
        );
      };
      kallipaiModule = import ../nixos-modules.nix {
        packages = stubPackages;
        aifedOverlay = aifed.overlays.default;
      };
      evalHost =
        extraModules:
        lib.nixosSystem {
          system = pkgs.stdenv.hostPlatform.system;
          modules = [
            kallipaiModule
            # Keep the eval quiet and the base assertions green: the stub
            # host pins a stateVersion and a dummy boot layout.
            {
              system.stateVersion = "26.11";
              boot.loader.grub.device = "/dev/null";
              fileSystems."/" = {
                device = "/dev/disk/by-label/stub";
                fsType = "ext4";
              };
            }
          ]
          ++ lib.toList extraModules;
        };
      aligned = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis.enable = true;
        };
      };
      # A non-default port evaluates clean and warning-free.
      drifted = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis.enable = true;
          polis.ports.lesche = 7250;
        };
      };
      failedAssertions = attrs: builtins.filter (a: !a.assertion) attrs.config.assertions;
      # The daemon block installs the whole workspace build on PATH.
      workspaceOnPath =
        builtins.elem stubPackages.${pkgs.stdenv.hostPlatform.system}.workspace
          aligned.config.environment.systemPackages;

      # The daemon block also installs the aifed tool on PATH, from
      # the option's default (the aifed input's own build). Asserting
      # the config's own value keeps the check self-contained.
      aifedOnPath = builtins.elem aligned.config.services.kallipai.aifedPackage aligned.config.environment.systemPackages;
      # The daemon unit's text must carry the system path on PATH: the
      # daemon resolves its helpers by bare name, and a NixOS unit's
      # PATH is empty unless the unit lists `path` explicitly. Dropping
      # the unit's path line loses the Environment line and this check
      # goes red.
      daemonUnitFile =
        pkgs.writeText "kallipai-daemon.service-test"
          aligned.config.systemd.units."kallipai-daemon.service".text;
      # The derived service defaults must reach the process: these two
      # files carry the archeion unit env for the derived (tls on) and
      # the tls-off hosts, and the assertions below grep them.
      webDerivedUnit =
        pkgs.writeText "kallipai-archeion-derived-test"
          webDerived.config.systemd.units."kallipai-archeion.service".text;
      webTlsOffUnit =
        pkgs.writeText "kallipai-archeion-tls-off-test"
          webTlsOff.config.systemd.units."kallipai-archeion.service".text;
      polisLescheUnit =
        pkgs.writeText "kallipai-lesche-test"
          webDerived.config.systemd.units."kallipai-lesche.service".text;
      polisFilesUnit =
        pkgs.writeText "kallipai-files-test"
          webDerived.config.systemd.units."kallipai-files.service".text;
      polisInstancesUnit =
        pkgs.writeText "kallipai-instances-test"
          webDerived.config.systemd.units."kallipai-instances.service".text;
      gatewayUnit =
        pkgs.writeText "kallipai-model-gateway-test"
          webDerived.config.systemd.units."kallipai-model-gateway.service".text;
      inherit (import ../lib.nix) bakeRuntimeConfig;
      stubDist = stubPackages.${pkgs.stdenv.hostPlatform.system}."kallipai-web-dist";
      # No runtime keys: the site root is the bundle itself.
      webPlain = evalHost {
        services.kallipai.web.enable = true;
      };
      # An explicit runtimeConfig key beats its platform-derived default.
      webCustom = evalHost {
        services.kallipai = {
          domain = "platform.example";
          web = {
            enable = true;
            runtimeConfig = {
              offlineLogin = false;
              domain = "kallipai.lan";
            };
          };
        };
      };
      # An unknown runtimeConfig key must fail the evaluation itself.
      webBogusKey = evalHost {
        services.kallipai.web = {
          enable = true;
          runtimeConfig.oflineLogin = false;
        };
      };
      bogusRejected =
        !(builtins.tryEval webBogusKey.config.services.kallipai.web.distWithRuntimeConfig.outPath).success;
      # The domain knob drives every web-facing default: the derived
      # CORS origin and the baked runtime config follow it, and tls off
      # flips both the scheme and the cookie. An explicit service option
      # beats its derived default.
      webDerived = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis.enable = true;
          web.enable = true;
          domain = "kallipai.com";
        };
      };
      # apiBase overrides every service base; the gateway admin URL
      # rides the same platform edge (one expression, two arms).
      webApiBase = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis.enable = true;
          web.enable = true;
          domain = "kallipai.com";
          web.runtimeConfig.apiBase = "https://edge.example.com";
        };
      };
      webTlsOff = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis.enable = true;
          web.enable = true;
          domain = "kallipai.com";
          tls = false;
        };
      };
      webOverride = evalHost {
        services.kallipai = {
          daemon.enable = true;
          domain = "kallipai.com";
          polis = {
            enable = true;
            archeion.corsOrigins = "https://custom.example";
            archeion.cookieSecure = false;
          };
        };
      };
      # The polis scrape export: empty with polis or gateway off; one
      # job per enabled metrics-capable service (the gateway today)
      # with the port following the service's own option, a drifted
      # management port reaching the target; the module leaves
      # services.prometheus untouched, and the documented wiring
      # (copied verbatim as a function module) evaluates green.
      scrapeOff = evalHost { services.kallipai.daemon.enable = true; };
      scrapeGwOff = evalHost {
        services.kallipai.polis = {
          enable = true;
          model-gateway.enable = false;
        };
      };
      scrapeJob = lib.elemAt aligned.config.services.kallipai.polis.scrapeConfigs 0;
      scrapeTarget = lib.elemAt (lib.elemAt scrapeJob.static_configs 0).targets 0;
      scrapeDrifted = evalHost {
        services.kallipai.polis = {
          enable = true;
          ports.model-gateway = 7510;
        };
      };
      scrapeDriftedTarget = lib.elemAt (lib.elemAt (lib.elemAt scrapeDrifted.config.services.kallipai.polis.scrapeConfigs 0).static_configs 0).targets 0;
      # Collision probes for the unified port check: one assertion per
      # colliding value group, the message naming every listener on the
      # value. collideFwd pins the forwarding face's explicit list entry;
      # gwOffCollide pins that a disabled gateway leaves the checked set.
      collide = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis = {
            enable = true;
            ports.model-gateway = 7200;
          };
        };
      };
      collideFwd = evalHost {
        services.kallipai = {
          daemon.enable = true;
          polis = {
            enable = true;
            model-gateway.forwardPort = 7100;
          };
        };
      };
      gwOffCollide = evalHost {
        services.kallipai.polis = {
          enable = true;
          model-gateway.enable = false;
          ports.model-gateway = 7200;
        };
      };
      collideMsg = (builtins.head (failedAssertions collide)).message;
      collideFwdMsg = (builtins.head (failedAssertions collideFwd)).message;
      # Pins that the documented example evaluates; wiring in stays the host's call.
      scrapeWired = evalHost [
        {
          services.kallipai.polis.enable = true;
        }
        (
          {
            config,
            ...
          }:
          {
            services.prometheus = {
              enable = true;
              scrapeConfigs = config.services.kallipai.polis.scrapeConfigs;
            };
          }
        )
      ];
      wiredTarget = lib.elemAt (lib.elemAt (lib.elemAt scrapeWired.config.services.prometheus.scrapeConfigs 0).static_configs 0).targets 0;

      # The baking helper, called directly (no module), writes exactly
      # the payload it is given.
      directBake = bakeRuntimeConfig {
        inherit pkgs;
        package = stubDist;
        runtimeConfig = {
          offlineLogin = false;
        };
      };
    in
    pkgs.runCommand "${project}-nixos-module-eval" { } ''
      # Forcing leaf options typechecks the module; assertions are
      # checked explicitly (they only throw in the toplevel activation).
      test "${toString aligned.config.services.kallipai.polis.ports.archeion}" = "7100"
      test "${toString aligned.config.services.kallipai.polis.ports.lesche}" = "7200"
      test "${toString (builtins.length (failedAssertions aligned))}" = "0"
      test "${toString (builtins.length aligned.config.warnings)}" = "0"
      test "${toString workspaceOnPath}" = "1"
      test "${toString aifedOnPath}" = "1"

      # The daemon unit rides the system path on PATH: bare-name
      # helper resolution depends on it (see daemonUnitFile).
      grep -q "${aligned.config.system.path}/bin" "${daemonUnitFile}"

      # The daemon socket literal lives in two places: the module
      # binding (unit env) and the client constant (the probe chain's
      # last leg). Pin both definition lines: editing either side
      # alone turns this check red.
      grep -q 'daemonSocket = "/run/kallipai/daemon.sock";' "${../nixos-modules.nix}"
      grep -q 'SYSTEM_DAEMON_SOCKET: &str = "/run/kallipai/daemon.sock";' "${../../crates/daemon/kallipai-daemon-common/src/socket.rs}"

      # A drifted polis-only host stays warning-free: no drift warning
      # exists since the subdomain shape hides ports behind the edge.
      test "${toString (builtins.length (failedAssertions drifted))}" = "0"
      test "${toString (builtins.length drifted.config.warnings)}" = "0"
      # The unified port check: a colliding host fails exactly one
      # assertion and the message names every listener on the value.
      test "${toString (builtins.length (failedAssertions collide))}" = "1"
      test "${
        toString (lib.hasInfix "ports.lesche" collideMsg && lib.hasInfix "ports.model-gateway" collideMsg)
      }" = "1"
      test "${toString (builtins.length (failedAssertions collideFwd))}" = "1"
      test "${
        toString (
          lib.hasInfix "ports.archeion" collideFwdMsg
          && lib.hasInfix "model-gateway.forwardPort" collideFwdMsg
        )
      }" = "1"
      test "${toString (builtins.length (failedAssertions gwOffCollide))}" = "0"

      # No runtime keys: the stub bundle passes straight through, shell
      # config.js and page sentinel both untouched.
      plain="${webPlain.config.services.kallipai.web.distWithRuntimeConfig}"
      test "$plain" = "${stubDist}"
      grep -q bundle-shell "$plain/config.js"
      grep -q bundle-page "$plain/index.html"

      # Custom runtime keys bake over the shell, user keys only; the
      # page sentinel still comes through.
      custom="${webCustom.config.services.kallipai.web.distWithRuntimeConfig}"
      grep -q '"offlineLogin":false' "$custom/config.js"
      grep -q '"domain":"kallipai.lan"' "$custom/config.js"
      grep -q bundle-page "$custom/index.html"

      # An unknown runtimeConfig key failed the evaluation itself.
      test "${toString bogusRejected}" = "1"

      # The domain knob drives the derived defaults; tls off flips the
      # scheme and the cookie; an explicit option beats the derivation.
      test "${webDerived.config.services.kallipai.polis.archeion.corsOrigins}" = "https://app.kallipai.com"
      test "${lib.boolToString webTlsOff.config.services.kallipai.polis.archeion.cookieSecure}" = "false"
      test "${webTlsOff.config.services.kallipai.polis.archeion.corsOrigins}" = "http://app.kallipai.com"
      test "${webTlsOff.config.services.kallipai.polis.archeion.webauthnRpId}" = "kallipai.com"
      test "${webOverride.config.services.kallipai.polis.archeion.corsOrigins}" = "https://custom.example"

      # The derived defaults must reach the process env: the rp origin
      # names the web page (the passkey ceremony runs there and the
      # archeion admits exactly that origin), tls-on leaves the cookie
      # flag to the code default, and tls-off forces it non-Secure.
      grep -q 'KALLIPAI_ARCHEION_WEBAUTHN_RP_ORIGIN=https://app.kallipai.com' '${webDerivedUnit}'
      grep -q 'KALLIPAI_ARCHEION_CORS_ORIGINS=https://app.kallipai.com' '${webDerivedUnit}'
      grep -q 'KALLIPAI_ARCHEION_OAUTH_REDIRECT_BASE=https://app.kallipai.com' '${webDerivedUnit}'
      test -z "$(grep KALLIPAI_ARCHEION_COOKIE_SECURE '${webDerivedUnit}')"
      grep -q 'KALLIPAI_ARCHEION_COOKIE_SECURE=false' '${webTlsOffUnit}'
      test "${lib.boolToString webOverride.config.services.kallipai.polis.archeion.cookieSecure}" = "false"

      # The gate-group handoff is single-tracked: the archeion unit runs
      # with the gate group as primary (systemd keeps the state tree
      # group-owned), consumers hold membership at the user layer, and
      # no unit carries a SupplementaryGroups re-declaration.
      grep -q 'Group=kallipai-polis' '${webDerivedUnit}'
      test -z "$(grep SupplementaryGroups '${webDerivedUnit}')"
      test -z "$(grep SupplementaryGroups '${polisLescheUnit}')"
      test -z "$(grep SupplementaryGroups '${polisFilesUnit}')"
      test -z "$(grep SupplementaryGroups '${polisInstancesUnit}')"
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallipai-lesche.extraGroups)}" = "1"
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallipai-files.extraGroups)}" = "1"
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallipai-instances.extraGroups)}" = "1"
      derived="${webDerived.config.services.kallipai.web.distWithRuntimeConfig}"
      grep -q '"domain":"kallipai.com"' "$derived/config.js"
      grep -q '"gatewayAdminUrl":"https://api.kallipai.com/v1/model-gateway"' "$derived/config.js"
      # The baking helper, called directly, writes exactly the payload.
      direct="${directBake}"
      grep -q '"offlineLogin":false' "$direct/config.js"
      test -z "$(grep bundle-shell "$direct/config.js")"
      grep -q bundle-page "$direct/index.html"

      # The gateway host: assertions all green (its port pair cannot
      # collide with the polis four at the defaults), the unit rides the
      # default ports on both faces, the peer-auth DB URL, the
      # domain-derived public base URL, and the log dir. No state
      # directory: durable surfaces live in PostgreSQL. The gateway's
      # ensure entries merge into the shared postgresql config beside
      # the polis ones (3 + 1 users).
      test "${toString (builtins.length (failedAssertions webDerived))}" = "0"
      grep -q 'KALLIPAI_MODEL_GATEWAY_ADDR=127.0.0.1:7501' '${gatewayUnit}'
      grep -q 'KALLIPAI_MODEL_GATEWAY_MANAGEMENT_ADDR=127.0.0.1:7500' '${gatewayUnit}'
      grep -q 'KALLIPAI_MODEL_GATEWAY_DATABASE_URL=postgresql:///kallipai-model-gateway?host=/run/postgresql' '${gatewayUnit}'
      grep -q 'KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL=https://model-gw.kallipai.com/v1' '${gatewayUnit}'
      grep -q 'KALLIPAI_MODEL_GATEWAY_LOG_DIR=/var/log/kallipai/model-gateway' '${gatewayUnit}'
      grep -q 'ExecStart=.*/bin/kallipai-model-gateway' '${gatewayUnit}'
      test -z "$(grep StateDirectory '${gatewayUnit}')"
      test -z "$(grep EnvironmentFile '${gatewayUnit}')"
      test "${toString (lib.elem "kallipai-model-gateway" webDerived.config.services.postgresql.ensureDatabases)}" = "1"
      test "${toString (builtins.length webDerived.config.services.postgresql.ensureUsers)}" = "4"
      # The admin face authenticates against the archeion: the internal
      # URL and the shared internal token file ride the unit env.
      grep -q 'KALLIPAI_MODEL_GATEWAY_ARCHEION_URL=http://127.0.0.1:7100' '${gatewayUnit}'
      grep -q 'KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE=/var/lib/kallipai/archeion/internal-token' '${gatewayUnit}'
      # The gateway user rides the platform gate group: the 0640
      # internal-token file is group-readable only through membership.
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallipai-model-gateway.extraGroups)}" = "1"
      # The domain derivation feeds the admin page CORS allowlist too.
      test "${webDerived.config.services.kallipai.polis.model-gateway.corsOrigins}" = "https://app.kallipai.com"
      # The gateway admin URL follows apiBase when the platform edge
      # moves (the default arm asserted beside the baked config.js).
      test "${webApiBase.config.services.kallipai.web.runtimeConfig.gatewayAdminUrl}" = "https://edge.example.com/v1/model-gateway"

      # The polis scrape export: no polis or gateway leaves the
      # list empty; a polis host gets one loopback job per
      # metrics-capable service with the port following the service's
      # own option; a drifted management port reaches the target; the
      # module keeps services.prometheus off, and the documented
      # wiring carries the job through verbatim. Stays assertion- and
      # warning-free beside the polis family.
      test "${toString (builtins.length scrapeOff.config.services.kallipai.polis.scrapeConfigs)}" = "0"
      test "${toString (builtins.length scrapeGwOff.config.services.kallipai.polis.scrapeConfigs)}" = "0"
      test "${lib.boolToString webDerived.config.services.prometheus.enable}" = "false"
      test "${toString (builtins.length aligned.config.services.kallipai.polis.scrapeConfigs)}" = "1"
      test "${scrapeJob.job_name}" = "kallipai-model-gateway"
      test "${scrapeJob.scrape_interval}" = "30s"
      test "${scrapeTarget}" = "127.0.0.1:7500"
      test "${scrapeDriftedTarget}" = "127.0.0.1:7510"
      test "${wiredTarget}" = "127.0.0.1:7500"
      test "${toString (builtins.length (failedAssertions scrapeWired))}" = "0"
      test "${toString (builtins.length scrapeWired.config.warnings)}" = "0"
      printf %s ok > "$out"
    '';
}
