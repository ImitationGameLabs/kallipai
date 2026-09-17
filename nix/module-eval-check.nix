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
                if name == "kallip-web-dist" then
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
              "kallip-daemon"
              "kallip-archeion"
              "kallip-lesche"
              "kallip-files"
              "kallip-instances"
              "kallip-web-dist"
              "workspace"
            ]
        );
      };
      kallipaiModule = import ./nixos-modules.nix {
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
            extraModules
          ];
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
        pkgs.writeText "kallip-daemon.service-test"
          aligned.config.systemd.units."kallip-daemon.service".text;
      # The derived service defaults must reach the process: these two
      # files carry the archeion unit env for the derived (tls on) and
      # the tls-off hosts, and the assertions below grep them.
      webDerivedUnit =
        pkgs.writeText "kallip-archeion-derived-test"
          webDerived.config.systemd.units."kallip-archeion.service".text;
      webTlsOffUnit =
        pkgs.writeText "kallip-archeion-tls-off-test"
          webTlsOff.config.systemd.units."kallip-archeion.service".text;
      polisLescheUnit =
        pkgs.writeText "kallip-lesche-test"
          webDerived.config.systemd.units."kallip-lesche.service".text;
      polisFilesUnit =
        pkgs.writeText "kallip-files-test"
          webDerived.config.systemd.units."kallip-files.service".text;
      polisInstancesUnit =
        pkgs.writeText "kallip-instances-test"
          webDerived.config.systemd.units."kallip-instances.service".text;
      inherit (import ./lib.nix) bakeRuntimeConfig;
      stubDist = stubPackages.${pkgs.stdenv.hostPlatform.system}."kallip-web-dist";
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
      grep -q 'daemonSocket = "/run/kallipai/daemon.sock";' "${./nixos-modules.nix}"
      grep -q 'SYSTEM_DAEMON_SOCKET: &str = "/run/kallipai/daemon.sock";' "${../crates/daemon/kallip-daemon-common/src/socket.rs}"
      # A drifted polis-only host stays warning-free: no drift warning
      # exists since the subdomain shape hides ports behind the edge.
      test "${toString (builtins.length (failedAssertions drifted))}" = "0"
      test "${toString (builtins.length drifted.config.warnings)}" = "0"
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
      grep -q 'KALLIP_ARCHEION_WEBAUTHN_RP_ORIGIN=https://app.kallipai.com' '${webDerivedUnit}'
      grep -q 'KALLIP_ARCHEION_CORS_ORIGINS=https://app.kallipai.com' '${webDerivedUnit}'
      grep -q 'KALLIP_ARCHEION_OAUTH_REDIRECT_BASE=https://app.kallipai.com' '${webDerivedUnit}'
      test -z "$(grep KALLIP_ARCHEION_COOKIE_SECURE '${webDerivedUnit}')"
      grep -q 'KALLIP_ARCHEION_COOKIE_SECURE=false' '${webTlsOffUnit}'
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
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallip-lesche.extraGroups)}" = "1"
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallip-files.extraGroups)}" = "1"
      test "${toString (lib.elem "kallipai-polis" webDerived.config.users.users.kallip-instances.extraGroups)}" = "1"
      derived="${webDerived.config.services.kallipai.web.distWithRuntimeConfig}"
      grep -q '"domain":"kallipai.com"' "$derived/config.js"
      # The baking helper, called directly, writes exactly the payload.
      direct="${directBake}"
      grep -q '"offlineLogin":false' "$direct/config.js"
      test -z "$(grep bundle-shell "$direct/config.js")"
      grep -q bundle-page "$direct/index.html"
      printf %s ok > "$out"
    '';
}
