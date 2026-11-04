# The host side of the observability stack: the NixOS module renders a tempo
# config text, and this check runs the tempo binary the unit runs against that
# exact file. The text is the one the store serves, read through the path the
# unit's ExecStart names, so a template that parses in a unit test but not in
# tempo 3.x fails here instead of at the first boot.
{
  pkgs,
  lib,
  aifed,
  project,
}:
let
  # The binaries each stubbed package stands for; the module's package
  # defaults resolve against these, so the eval never builds a crate.
  stubBins = {
    kallipai-host = [
      "kallipai-daemon"
      "kallipai-daemon-spawn"
      "kallipctl"
      "kallipai-tagma"
      "kallipai-run"
      "kallip"
    ];
    workspace = [
      "kallipai-daemon"
      "kallipai-daemon-spawn"
      "kallipctl"
      "kallipai-tagma"
      "kallipai-run"
      "kallip"
      "kallipai-archeion"
      "kallipai-lesche"
      "kallipai-keryx"
      "kallipai-files"
      "kallipai-instances"
      "kallipai-model-gateway"
      "kallipai-admin"
    ];
    kallipai-daemon = [ "kallipai-daemon" ];
    kallipai-daemon-spawn = [ "kallipai-daemon-spawn" ];
    kallipctl = [ "kallipctl" ];
    kallipai-tagma = [
      "kallipai-tagma"
      "kallip"
    ];
    kallipai-run = [ "kallipai-run" ];
    kallipai-archeion = [ "kallipai-archeion" ];
    kallipai-lesche = [ "kallipai-lesche" ];
    kallipai-keryx = [ "kallipai-keryx" ];
    kallipai-files = [ "kallipai-files" ];
    kallipai-instances = [ "kallipai-instances" ];
    kallipai-model-gateway = [ "kallipai-model-gateway" ];
    kallipai-admin = [ "kallipai-admin" ];
  };
  stubPackages = {
    ${pkgs.stdenv.hostPlatform.system} =
      builtins.mapAttrs (
        name: bins:
        pkgs.runCommand "${name}-stub" { } ''
          mkdir -p $out/bin
          for binname in ${lib.concatStringsSep " " bins}; do
            echo '${name}' > $out/bin/$binname
            chmod +x $out/bin/$binname
          done
        ''
      ) stubBins
      // {
        kallipai-web-dist = pkgs.runCommand "kallipai-web-dist-stub" { } ''
          mkdir $out
          echo bundle-shell > $out/config.js
          echo bundle-page > $out/index.html
        '';
      };
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
  polisHost = evalHost {
    services.kallipai.polis.enable = true;
  };
  # A second host with a different trace budget: the config text has to follow
  # the option, so a hardcoded horizon fails the grep below.
  shortHost = evalHost {
    services.kallipai = {
      polis.enable = true;
      observability.retention.traces = 24;
    };
  };
  tempoUnit = host: host.config.systemd.services.kallipai-tempo;
in
{
  "${project}-obs-nixos" =
    pkgs.runCommand "${project}-obs-nixos"
      {
        # The units' ExecStart strings, carried whole: a config path pulled out
        # of one with a Nix string operation loses its store context, and the
        # file would then be missing from the sandbox. The path is cut in the
        # shell instead, so the derivation stays a build input.
        tempoExecPolis = (tempoUnit polisHost).serviceConfig.ExecStart;
        tempoExecShort = (tempoUnit shortHost).serviceConfig.ExecStart;
      }
      ''
        # The unit shape: the account, the state directory, and an ExecStart
        # that names the binary and a config file.
        tempoUnitLine() {
          test "$1" = "$2" || {
            echo "unit: expected '$2', read '$1'" >&2
            exit 1
          }
        }
        tempoUnitLine "${(tempoUnit polisHost).serviceConfig.User}" "kallipai-tempo"
        tempoUnitLine "${(tempoUnit polisHost).serviceConfig.Group}" "kallipai-tempo"
        tempoUnitLine "${(tempoUnit polisHost).serviceConfig.StateDirectory}" "kallipai/telemetry/tempo"
        tempoUnitLine "${(tempoUnit polisHost).serviceConfig.StateDirectoryMode}" "0700"
        tempoUnitLine "${lib.boolToString (lib.hasInfix "-config.file=" (tempoUnit polisHost).serviceConfig.ExecStart)}" "true"

        configOf() {
          printf '%s\n' "$1" | sed -e 's/.*-config\.file=//' -e 's/ .*//'
        }
        tempoConfigPolis=$(configOf "$tempoExecPolis")
        tempoConfigShort=$(configOf "$tempoExecShort")
        test -f "$tempoConfigPolis"
        test -f "$tempoConfigShort"

        # The text parses in the binary the unit runs.
        ${pkgs.tempo}/bin/tempo -config.file="$tempoConfigPolis" -config.verify=true
        ${pkgs.tempo}/bin/tempo -config.file="$tempoConfigShort" -config.verify=true

        # Loopback only, on the ports the options carry.
        grep -q 'http_listen_address: 127.0.0.1' "$tempoConfigPolis"
        grep -q 'grpc_listen_address: 127.0.0.1' "$tempoConfigPolis"
        grep -q 'http_listen_port: ${toString polisHost.config.services.kallipai.observability.ports.tempo}' "$tempoConfigPolis"
        grep -q 'endpoint: 127.0.0.1:${toString polisHost.config.services.kallipai.observability.ports.otlp}' "$tempoConfigPolis"
        grep -q 'endpoint: 127.0.0.1:${toString polisHost.config.services.kallipai.observability.ports.otlp-grpc}' "$tempoConfigPolis"
        # The trace budget follows the option; the default is the compose
        # stack's 72-hour pruning budget.
        grep -q 'prune_age: ${toString polisHost.config.services.kallipai.observability.retention.traces}h' "$tempoConfigPolis"
        grep -q 'prune_age: 24h' "$tempoConfigShort"
        # The store the pruner walks: the subtree systemd creates for the unit.
        grep -q 'path: /var/lib/kallipai/telemetry/tempo' "$tempoConfigPolis"
        # The live store's WAL and shutdown marker stay inside the state
        # directory too: their defaults sit under /var/tempo, which the unit's
        # account does not own, and tempo refuses to start over that.
        grep -q 'shutdown_marker_dir: /var/lib/kallipai/telemetry/tempo/live-store/shutdown-marker' "$tempoConfigPolis"
        grep -q 'path: /var/lib/kallipai/telemetry/tempo/live-store/traces' "$tempoConfigPolis"

        mkdir -p $out
        cp "$tempoConfigPolis" $out/tempo.yaml
        echo verified > $out/verified.txt
      '';
}
