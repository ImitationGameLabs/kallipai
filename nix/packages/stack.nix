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
# tempo.yaml, grafana.ini, grafana-datasources.yaml and prometheus.yaml.

{ lib, pkgs }:
let
  obs = import ../obs-stack.nix { inherit lib; };
  shared = import ../platform-texts.nix { inherit lib; };

  # The deployment's instance table: this renderer and scripts/compose-up.sh
  # read one list, so an intake port or a token path is configured once and
  # spelled nowhere else. An absent file means no instance has its own intake,
  # which is the stack every deployment starts with.
  #
  # The table is configuration, not source: it names host paths that differ per
  # deployment, so it is not tracked, and a flake's source copy cannot carry it.
  # The script exports where it is and builds with --impure, the one read that
  # leaves the store behind; the variable name is shared with the script, and
  # the check reads it back from both.
  instancesFile =
    let
      fromEnvironment = builtins.getEnv "KALLIPAI_TELEMETRY_INSTANCES_FILE";
    in
    if fromEnvironment != "" then fromEnvironment else toString ../../compose/telemetry-instances.json;
  instances =
    if builtins.pathExists instancesFile then
      builtins.fromJSON (builtins.readFile instancesFile)
    else
      [ ];

  # Only the texts are wanted here: they are plain strings, so no host ports
  # and no data root are involved.
  stack = obs.mkStack { inherit instances; };

  texts = {
    "otelcol.yaml" = pkgs.writeText "otelcol.yaml" stack.texts.otelcol;
    "tempo.yaml" = pkgs.writeText "tempo.yaml" stack.texts.tempo;
    "grafana.ini" = pkgs.writeText "grafana.ini" stack.texts.grafana;
    "grafana-datasources.yaml" = pkgs.writeText "grafana-datasources.yaml" stack.texts.datasource;
    "prometheus.yaml" = pkgs.writeText "prometheus.yaml" shared.prometheus;
  }
  # The agent runs beside the host's network namespace rather than on the
  # compose network: no compose service mounts it, and the check keeps it out of
  # the set compose must mount. scripts/compose-up.sh starts the container that
  # reads it, from this build, so its ports come from the same table.
  // lib.optionalAttrs (stack.texts.agent != null) {
    "agent.yaml" = pkgs.writeText "agent.yaml" stack.texts.agent;
  };
in
# The names travel with the output so the eval check can compare them with
# the volume lines in compose, the contract that breaks silently.
pkgs.runCommand "kallipai-stack"
  {
    passthru = {
      textNames = builtins.attrNames texts;
      # What is written for a host process rather than a container: compose must
      # not mount these, and the check names them apart from the mounted set.
      hostOnlyTextNames = lib.optional (stack.texts.agent != null) "agent.yaml";
    };
  }
  (
    ''
      mkdir -p $out
    ''
    + lib.concatStrings (
      lib.mapAttrsToList (name: path: ''
        install -m 0444 ${path} $out/${name}
      '') texts
    )
  )
