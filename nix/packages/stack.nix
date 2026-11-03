# The config texts the docker compose stack mounts, as one store directory:
# KALLIPAI_STACK_DIR points at it and compose.yaml/dev mounts single files out
# of it read-only.
#
# The four observability texts are not written here. They come from
# nix/obs-stack.nix, so the collector, the trace store and the Grafana
# provisioning are described once and rendered here. prometheus.yaml comes from
# nix/platform-texts.nix. The edge renders nothing: its TLS pair is a mount and
# its certificate paths are environment values.
#
# Names are fixed by the volume lines in compose.yaml: otelcol.yaml,
# tempo.yaml, grafana.ini, grafana-tempo-datasource.yaml and prometheus.yaml.

{ lib, pkgs }:
let
  obs = import ../obs-stack.nix { inherit lib; };
  shared = import ../platform-texts.nix { inherit lib; };

  # Only the texts are wanted here: they are plain strings, so no host ports
  # and no data root are involved.
  stack = obs.mkStack { };

  texts = {
    "otelcol.yaml" = pkgs.writeText "otelcol.yaml" stack.texts.otelcol;
    "tempo.yaml" = pkgs.writeText "tempo.yaml" stack.texts.tempo;
    "grafana.ini" = pkgs.writeText "grafana.ini" stack.texts.grafana;
    "grafana-tempo-datasource.yaml" =
      pkgs.writeText "grafana-tempo-datasource.yaml" stack.texts.datasource;
    "prometheus.yaml" = pkgs.writeText "prometheus.yaml" shared.prometheus;
  };
in
# The names travel with the output so the eval check can compare them with
# the volume lines in compose, the contract that breaks silently.
pkgs.runCommand "kallipai-stack" { passthru.textNames = builtins.attrNames texts; } (
  ''
    mkdir -p $out
  ''
  + lib.concatStrings (
    lib.mapAttrsToList (name: path: ''
      install -m 0444 ${path} $out/${name}
    '') texts
  )
)
