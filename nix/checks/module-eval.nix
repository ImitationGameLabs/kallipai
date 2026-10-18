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
      # The app face origin as the platform documents it, recomposed from
      # the public knobs: the checks below compare the derived options and
      # the rendered unit env against this, never against a literal.
      schemeFor = cfg: if cfg.config.services.kallipai.tls then "https" else "http";
      appOriginFor = cfg: "${schemeFor cfg}://app.${cfg.config.services.kallipai.domain}";
      # The gateway admin face's public base, composed from the same knobs.
      gatewayBaseFor = cfg: "${schemeFor cfg}://model-gw.${cfg.config.services.kallipai.domain}/v1";
      # The api edge's origin, composed from the same knobs: the default
      # arm of the gateway admin URL rides it.
      apiEdgeFor = cfg: "${schemeFor cfg}://api.${cfg.config.services.kallipai.domain}";
      # The six polis port defaults in the module's own order (archeion,
      # lesche, keryx, files, instances, model-gateway), plus the
      # forwarding face's port, which is a separate binding. Each pin pair
      # compares one space-separated string of numbers: the evaluated
      # options against it, then the module file's binding as written, so
      # a default change lands here instead of drifting silently past the
      # crates, the images and the compose files. Operators meet these
      # numbers across docs/en/deployment/container.md,
      # docs/en/deployment/nixos/minimal.md and
      # docs/en/development/setup.md, except the instances port, which
      # sits in no page under docs/. This pin reads the module, not those
      # documents, so a port change has to reach them by hand.
      portNames = [
        "archeion"
        "lesche"
        "keryx"
        "files"
        "instances"
        "model-gateway"
      ];
      portsLine = ports: lib.concatStringsSep " " (map (n: toString ports.${n}) portNames);
      pinnedPorts = "7100 7200 7210 7400 7300 7500";
      pinnedForwardPort = "7501";
      # nix/nixos-modules.nix binds these defaults once (defaultPolisPorts
      # for the six, defaultForwardPort for the forwarding face); the pin
      # reads those lines back instead of repeating the numbers again.
      moduleBinding =
        name:
        let
          lines = lib.splitString "\n" (builtins.readFile ../nixos-modules.nix);
          hits = builtins.filter (m: m != null) (
            map (l: builtins.match "[[:space:]]*${name} = ([0-9]+);" l) lines
          );
        in
        if builtins.length hits == 0 then
          throw "module binding ${name}: no candidate line in nix/nixos-modules.nix"
        else if builtins.length hits > 1 then
          throw "module binding ${name}: ${toString (builtins.length hits)} lines in nix/nixos-modules.nix, want exactly one"
        else
          builtins.head (builtins.head hits);
      boundPortsLine = lib.concatStringsSep " " (map moduleBinding portNames);
      boundForwardPortLine = moduleBinding "defaultForwardPort";
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
      polisKeryxUnit =
        pkgs.writeText "kallipai-keryx-test"
          webDerived.config.systemd.units."kallipai-keryx.service".text;
      polisFilesUnit =
        pkgs.writeText "kallipai-files-test"
          webDerived.config.systemd.units."kallipai-files.service".text;
      polisInstancesUnit =
        pkgs.writeText "kallipai-instances-test"
          webDerived.config.systemd.units."kallipai-instances.service".text;
      gatewayUnit =
        pkgs.writeText "kallipai-model-gateway-test"
          webDerived.config.systemd.units."kallipai-model-gateway.service".text;
      # Every edge service's unit env must carry the derived CORS
      # allowlist; the roster below drives the assertions in the script.
      corsEnvUnits = [
        {
          env = "KALLIPAI_ARCHEION_CORS_ORIGINS";
          unit = webDerivedUnit;
        }
        {
          env = "KALLIPAI_LESCHE_CORS_ORIGINS";
          unit = polisLescheUnit;
        }
        {
          env = "KALLIPAI_KERYX_CORS_ORIGINS";
          unit = polisKeryxUnit;
        }
        {
          env = "KALLIPAI_FILES_CORS_ORIGINS";
          unit = polisFilesUnit;
        }
        {
          env = "KALLIPAI_INSTANCES_CORS_ORIGINS";
          unit = polisInstancesUnit;
        }
        {
          env = "KALLIPAI_MODEL_GATEWAY_CORS_ORIGINS";
          unit = gatewayUnit;
        }
      ];
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
      # The instance-host declaration the module writes: a polis-only
      # deployment runs the instances API but no process half, so it
      # declares false; an explicit value beats the derivation. The
      # mirror case: a daemon-only deployment serves no instances API
      # either, so the derivation writes false the same way.
      webNoBackend = evalHost {
        services.kallipai = {
          polis.enable = true;
          web.enable = true;
        };
      };
      webHostOverride = evalHost {
        services.kallipai = {
          polis.enable = true;
          web = {
            enable = true;
            runtimeConfig.instancesServiceEnabled = true;
          };
        };
      };
      webDaemonOnly = evalHost {
        services.kallipai = {
          daemon.enable = true;
          web.enable = true;
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
      gatewayDbName = "kallipai-model-gateway";
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
      # value. collideFwd pins the forwarding port against a listener's
      # own port; gwOffCollide pins that a disabled gateway leaves the
      # checked set.
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
            ports.archeion = 7100;
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
      directPayload = {
        offlineLogin = false;
      };
      directBake = bakeRuntimeConfig {
        inherit pkgs;
        package = stubDist;
        runtimeConfig = directPayload;
      };
    in
    pkgs.runCommand "${project}-nixos-module-eval" { } ''
      # Forcing leaf options typechecks the module; assertions are
      # checked explicitly (they only throw in the toplevel activation).
      # The port pins: see pinnedPorts and pinnedForwardPort above.
      test "${portsLine aligned.config.services.kallipai.polis.ports}" = "${pinnedPorts}"
      test "${boundPortsLine}" = "${pinnedPorts}"
      test "${toString aligned.config.services.kallipai.polis.model-gateway.forwardPort}" = "${pinnedForwardPort}"
      test "${boundForwardPortLine}" = "${pinnedForwardPort}"
      test "${toString (builtins.length (failedAssertions aligned))}" = "0"
      test "${toString (builtins.length aligned.config.warnings)}" = "0"
      test "${toString workspaceOnPath}" = "1"
      test "${toString aifedOnPath}" = "1"

      # The daemon unit rides the system path on PATH: bare-name
      # helper resolution depends on it (see daemonUnitFile).
      grep -q "${aligned.config.system.path}/bin" "${daemonUnitFile}"

      # The daemon socket lives in two places: the module binding (unit
      # env) and the client constant (the probe chain's last leg). Read
      # both sides and compare: moving the path on one side alone turns
      # this check red. Moving both at once would not: the path is also
      # published for operators in docs/en/deployment/nixos/minimal.md
      # and docs/zh-cn/deployment/nixos/minimal.md, and this check reads
      # neither.
      daemonSock=$(grep -o 'KALLIPAI_DAEMON_SOCKET=[^ "]*' '${daemonUnitFile}' | cut -d= -f2)
      clientSock=$(grep -o 'SYSTEM_DAEMON_SOCKET: &str = "[^"]*"' '${../../crates/daemon/kallipai-daemon-common/src/socket.rs}' | cut -d '"' -f2)
      test -n "$clientSock"
      test -n "$daemonSock"
      test "$daemonSock" = "$clientSock"

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
      grep -q "\"offlineLogin\":${builtins.toJSON webCustom.config.services.kallipai.web.runtimeConfig.offlineLogin}" "$custom/config.js"
      grep -q "\"domain\":\"${webCustom.config.services.kallipai.web.runtimeConfig.domain}\"" "$custom/config.js"
      grep -q bundle-page "$custom/index.html"

      # The instance-host declaration the module derives: a polis-only
      # deployment runs the instances API but no process half, so it
      # declares false; an explicit value beats the derivation. The
      # no-key arm above stays straight-through either way: a deployment
      # that runs neither polis nor the daemon is told nothing (shipped
      # default).
      test '${builtins.toJSON webNoBackend.config.services.kallipai.web.runtimeConfig.instancesServiceEnabled}' = 'false'
      test '${builtins.toJSON webHostOverride.config.services.kallipai.web.runtimeConfig.instancesServiceEnabled}' = 'true'
      noBackend=${webNoBackend.config.services.kallipai.web.distWithRuntimeConfig}
      grep -q '"instancesServiceEnabled":false' "$noBackend/config.js"
      hostOverride=${webHostOverride.config.services.kallipai.web.distWithRuntimeConfig}
      grep -q '"instancesServiceEnabled":true' "$hostOverride/config.js"
      daemonOnly=${webDaemonOnly.config.services.kallipai.web.distWithRuntimeConfig}
      test '${builtins.toJSON webDaemonOnly.config.services.kallipai.web.runtimeConfig.instancesServiceEnabled}' = 'false'
      grep -q '"instancesServiceEnabled":false' "$daemonOnly/config.js"
      test '${toString (builtins.length (failedAssertions webNoBackend))}' = '0'
      test '${toString (builtins.length webNoBackend.config.warnings)}' = '0'
      test '${toString (builtins.length (failedAssertions webDaemonOnly))}' = '0'
      test '${toString (builtins.length webDaemonOnly.config.warnings)}' = '0'
      test '${toString (builtins.length (failedAssertions webHostOverride))}' = '0'
      test '${toString (builtins.length webHostOverride.config.warnings)}' = '0'

      # An unknown runtimeConfig key failed the evaluation itself.
      test "${toString bogusRejected}" = "1"

      # The domain knob drives the derived defaults; tls off flips the
      # scheme and the cookie; an explicit option beats the derivation.
      test "${webDerived.config.services.kallipai.polis.archeion.corsOrigins}" = "${appOriginFor webDerived}"
      test "${lib.boolToString webTlsOff.config.services.kallipai.polis.archeion.cookieSecure}" = "${lib.boolToString webTlsOff.config.services.kallipai.tls}"
      test "${webTlsOff.config.services.kallipai.polis.archeion.corsOrigins}" = "${appOriginFor webTlsOff}"
      test "${webTlsOff.config.services.kallipai.polis.archeion.webauthnRpId}" = "${webTlsOff.config.services.kallipai.domain}"
      test "${webDerived.config.services.kallipai.polis.archeion.webauthnRpOrigin}" = "${appOriginFor webDerived}"
      test "${webDerived.config.services.kallipai.polis.archeion.oauthRedirectBase}" = "${appOriginFor webDerived}"
      test "${webOverride.config.services.kallipai.polis.archeion.corsOrigins}" = "https://custom.example"
      # A second domain proves the derivation reads the knob rather than
      # a built-in name: webCustom carries platform.example.
      test "${webCustom.config.services.kallipai.polis.archeion.corsOrigins}" = "${appOriginFor webCustom}"

      # The derived defaults must reach the process env: the rp origin
      # names the web page (the passkey ceremony runs there and the
      # archeion admits exactly that origin), tls-on leaves the cookie
      # flag to the code default, and tls-off forces it non-Secure.
      grep -q "KALLIPAI_ARCHEION_WEBAUTHN_RP_ORIGIN=${webDerived.config.services.kallipai.polis.archeion.webauthnRpOrigin}" '${webDerivedUnit}'
      grep -q "KALLIPAI_ARCHEION_OAUTH_REDIRECT_BASE=${webDerived.config.services.kallipai.polis.archeion.oauthRedirectBase}" '${webDerivedUnit}'
      test -z "$(grep KALLIPAI_ARCHEION_COOKIE_SECURE '${webDerivedUnit}')"
      grep -q "KALLIPAI_ARCHEION_COOKIE_SECURE=${lib.boolToString webTlsOff.config.services.kallipai.polis.archeion.cookieSecure}" '${webTlsOffUnit}'
      test "${lib.boolToString webOverride.config.services.kallipai.polis.archeion.cookieSecure}" = "false"

      # The polis gate is the primary group everywhere it matters:
      # every service unit runs Group=kallipai-polis (systemd keeps
      # the state and log trees group-owned inside the trust domain),
      # the accounts carry the same primary, and no unit carries a
      # SupplementaryGroups re-declaration.
      grep -q 'Group=kallipai-polis' '${webDerivedUnit}'
      grep -q 'Group=kallipai-polis' '${polisLescheUnit}'
      grep -q 'Group=kallipai-polis' '${polisKeryxUnit}'
      grep -q 'Group=kallipai-polis' '${polisFilesUnit}'
      grep -q 'Group=kallipai-polis' '${polisInstancesUnit}'
      test -z "$(grep SupplementaryGroups '${webDerivedUnit}')"
      test -z "$(grep SupplementaryGroups '${polisLescheUnit}')"
      test -z "$(grep SupplementaryGroups '${polisKeryxUnit}')"
      test -z "$(grep SupplementaryGroups '${polisFilesUnit}')"
      test -z "$(grep SupplementaryGroups '${polisInstancesUnit}')"
      test "${webDerived.config.users.users.kallipai-lesche.group}" = "kallipai-polis"
      test "${webDerived.config.users.users.kallipai-keryx.group}" = "kallipai-polis"
      test "${webDerived.config.users.users.kallipai-files.group}" = "kallipai-polis"
      test "${webDerived.config.users.users.kallipai-instances.group}" = "kallipai-polis"
      # The daemon socket gate is its own group (kallipai): the daemon
      # unit hands the socket to it and only the instances account
      # joins it at the user layer.
      grep -q 'KALLIPAI_DAEMON_SOCKET_GROUP=kallipai' '${daemonUnitFile}'
      test "${toString (lib.elem "kallipai" webDerived.config.users.users.kallipai-instances.extraGroups)}" = "1"
      # Every edge service's unit env carries the derived CORS allowlist;
      # one roster drives the six greps, so a new service needs one line.
      ${lib.concatMapStringsSep "\n      " (
        p: "grep -q '${p.env}=${appOriginFor webDerived}' '${toString p.unit}'"
      ) corsEnvUnits}
      # The keryx unit: the relay binary on ExecStart and the
      # no-database contract (all state is in-memory, so the unit env
      # carries no DATABASE_URL at all).
      grep -q 'ExecStart=.*/bin/kallipai-keryx' '${polisKeryxUnit}'
      test -z "$(grep DATABASE_URL '${polisKeryxUnit}')"
      derived="${webDerived.config.services.kallipai.web.distWithRuntimeConfig}"
      # The baked domain comes from the platform knob through the module's
      # mkDefault, so a derivation that hardcodes it turns this red.
      grep -q "\"domain\":\"${webDerived.config.services.kallipai.domain}\"" "$derived/config.js"
      grep -q "\"gatewayAdminUrl\":\"${webDerived.config.services.kallipai.web.runtimeConfig.gatewayAdminUrl}\"" "$derived/config.js"
      grep -q '"instancesServiceEnabled":true' "$derived/config.js"
      # The baking helper, called directly, writes exactly the payload.
      direct="${directBake}"
      grep -q "\"offlineLogin\":${builtins.toJSON directPayload.offlineLogin}" "$direct/config.js"
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
      grep -q "KALLIPAI_MODEL_GATEWAY_ADDR=127.0.0.1:${toString webDerived.config.services.kallipai.polis.model-gateway.forwardPort}" '${gatewayUnit}'
      grep -q "KALLIPAI_MODEL_GATEWAY_MANAGEMENT_ADDR=127.0.0.1:${toString webDerived.config.services.kallipai.polis.ports.model-gateway}" '${gatewayUnit}'
      gatewayDb=$(grep -o 'KALLIPAI_MODEL_GATEWAY_DATABASE_URL=[^ "]*' '${gatewayUnit}' | cut -d= -f2-)
      test -n "$gatewayDb"
      # The gateway carries its own database name on the archeion socket
      # query string: read that query once, then compare the two legs.
      archeionDbQuery=$(grep -o 'KALLIPAI_ARCHEION_DATABASE_URL=[^ "]*' '${webDerivedUnit}' | cut -d= -f2- | cut -d'?' -f2)
      test -n "$archeionDbQuery"
      test "$gatewayDb" = "postgresql:///${gatewayDbName}?$archeionDbQuery"
      test "${webDerived.config.services.kallipai.polis.model-gateway.publicBaseUrl}" = "${gatewayBaseFor webDerived}"
      grep -q "KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL=${webDerived.config.services.kallipai.polis.model-gateway.publicBaseUrl}" '${gatewayUnit}'
      polisLogDir=$(grep -o 'KALLIPAI_ARCHEION_LOG_DIR=[^ "]*' '${webDerivedUnit}' | cut -d= -f2)
      test -n "$polisLogDir"
      polisLogRoot=$(dirname "$polisLogDir")
      grep -q "KALLIPAI_MODEL_GATEWAY_LOG_DIR=$polisLogRoot/model-gateway" '${gatewayUnit}'
      grep -q 'ExecStart=.*/bin/kallipai-model-gateway' '${gatewayUnit}'
      test -z "$(grep StateDirectory '${gatewayUnit}')"
      test -z "$(grep EnvironmentFile '${gatewayUnit}')"
      test "${toString (lib.elem "kallipai-model-gateway" webDerived.config.services.postgresql.ensureDatabases)}" = "1"
      test "${toString (builtins.length webDerived.config.services.postgresql.ensureUsers)}" = "4"
      # The admin face authenticates against the archeion: the internal
      # URL and the shared internal token file ride the unit env.
      archeionAddr=$(grep -o 'KALLIPAI_ARCHEION_ADDR=[^ "]*' '${webDerivedUnit}' | cut -d= -f2)
      test -n "$archeionAddr"
      grep -q "KALLIPAI_MODEL_GATEWAY_ARCHEION_URL=http://$archeionAddr" '${gatewayUnit}'
      archeionToken=$(grep -o 'KALLIPAI_ARCHEION_INTERNAL_TOKEN_FILE=[^ "]*' '${webDerivedUnit}' | cut -d= -f2)
      test -n "$archeionToken"
      grep -q "KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE=$archeionToken" '${gatewayUnit}'
      # The gateway user's primary group is the platform gate: the
      # 0640 internal-token file is group-readable through it.
      test "${webDerived.config.users.users.kallipai-model-gateway.group}" = "kallipai-polis"
      # The domain derivation feeds the admin page CORS allowlist too.
      test "${webDerived.config.services.kallipai.polis.model-gateway.corsOrigins}" = "${appOriginFor webDerived}"
      # The gateway admin URL follows apiBase when the platform edge
      # moves (the default arm asserted beside the baked config.js).
      test "${webApiBase.config.services.kallipai.web.runtimeConfig.gatewayAdminUrl}" = "${webApiBase.config.services.kallipai.web.runtimeConfig.apiBase}/v1/model-gateway"
      # The default arm: with no apiBase override the admin URL rides the
      # platform domain's api edge.
      test "${webDerived.config.services.kallipai.web.runtimeConfig.gatewayAdminUrl}" = "${apiEdgeFor webDerived}/v1/model-gateway"

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
      # The job name is a dashboard-side label (Grafana panels select on
      # it), so it is pinned by value beside the unit-existence check.
      test "${scrapeJob.job_name}" = "kallipai-model-gateway"
      test "${toString (builtins.hasAttr "${scrapeJob.job_name}.service" webDerived.config.systemd.units)}" = "1"
      test "${scrapeJob.scrape_interval}" = "30s"
      test "${scrapeTarget}" = "127.0.0.1:${toString aligned.config.services.kallipai.polis.ports.model-gateway}"
      test "${scrapeDriftedTarget}" = "127.0.0.1:${toString scrapeDrifted.config.services.kallipai.polis.ports.model-gateway}"
      test "${wiredTarget}" = "127.0.0.1:${toString scrapeWired.config.services.kallipai.polis.ports.model-gateway}"
      test "${toString (builtins.length (failedAssertions scrapeWired))}" = "0"
      test "${toString (builtins.length scrapeWired.config.warnings)}" = "0"
      printf %s ok > "$out"
    '';
}
