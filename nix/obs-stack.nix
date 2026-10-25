# The local observability stack for the dev compose: a collector, a trace
# store and a UI. Written as a pure function -- no `services.*` references --
# so `nix flake check` can evaluate it without arion, which is not a flake
# input. compose/dev/polis.nix assigns the result onto arion's options; that
# assignment is the one part no check can prove -- arion is not a
# flake input, so only arion up sees it fail.
#
# What it starts
#   otelcol  otelcol-contrib: the OTLP receiver (http published to the host,
#            grpc inside the compose network), forwarding traces to tempo and
#            answering health probes.
#   tempo    the trace store: query API published on loopback, OTLP received
#            inside the network only; blocks retained for the age budget below.
#   grafana  the UI, published on loopback, anonymous viewer, tempo
#            pre-provisioned as the default datasource so a trace id pastes
#            straight into Explore. Its host port stays clear of the dev tagma,
#            which binds a port of its own on the host network
#            (compose/dev/tagma.nix).
#
# Which port carries which role lives in the two tables below, not here:
# `defaults` is the host side the user may move, `containerPorts` is what the
# processes listen on and what every text and publish mapping is built from.
#
# Where the data lands (outside the project, so `arion down -v` cannot eat it)
#   <telemetryRoot>/tempo     blocks, wal
#   <telemetryRoot>/grafana   grafana.db, plugins, logs
#   <telemetryRoot> is $XDG_DATA_HOME/kallipai/telemetry, falling back to
#   $HOME/.local/share/kallipai/telemetry. The stack writes there and the
#   prune tool reads the same value, so cleaning and writing cannot disagree.
#
# How to clear it
#   kallipai-telemetry-prune            dry run: prints what it would delete
#   kallipai-telemetry-prune --apply    same rules on a later run; reports every skip
#   rm -rf <telemetryRoot>              everything, with the stack stopped
#
# These two numbers are mirrored in crates/kallipai-telemetry-prune/src/lib.rs
# (MAX_BYTES, MAX_AGE_HOURS). nix/checks/obs-stack.nix reads that source back
# and compares the pairs, so a change to one copy without the other fails the
# check rather than leaving the two tools on different budgets. They stay
# build-time constants so a wrong budget fails evaluation first.
{ lib }:
let
  maxBytes = 5 * 1024 * 1024 * 1024;
  maxAgeHours = 72;

  # Every container port the config texts and the publish mappings name. The
  # host side is `defaults` below: the user may move that one, while these are
  # what the processes listen on and what polis.nix builds each endpoint from.
  # 4317 is spelled once because the collector's receive side and the address it
  # forwards to move together here; the two containers are otherwise independent,
  # so a second entry is the edit if they ever diverge.
  containerPorts = {
    otlpHTTP = "4318";
    otlpGRPC = "4317";
    otelHealth = "13133";
    tempo = "3200";
    grafana = "3000";
  };

  # The same table under a second name: a formal argument cannot default to a
  # variable of its own name, and `mkStack`'s container port parameter has to
  # default to this one.
  defaultContainerPorts = containerPorts;

  # Every host path this stack touches goes through here: absolute,
  # colon-free (docker's bind syntax splits on ':'), never '/'.
  # The prune tool guards less on purpose (an absolute path is enough there):
  # a base rejected here for a colon or a bare '/' reaches it as a root that
  # does not exist, so a run reports a no-op; a relative base fails the
  # absolute check on that side instead of passing quietly.
  guardedPath =
    name: path:
    if path == "/" || !(lib.hasPrefix "/" path) || lib.hasInfix ":" path then
      throw "obs-stack: ${name} must be an absolute, colon-free host path other than '/' (got '${path}')"
    else
      path;
in
{
  inherit maxBytes maxAgeHours containerPorts;

  # The four host ports this stack publishes, and their defaults. Both compose
  # files read them from here, so the collector's address cannot drift between
  # two independent `arion` evaluations. Every one is loopback-only, and a host
  # override moves the published side only: the container side is a value, not
  # a literal, in every text and publish mapping below.
  defaults = {
    otlpHTTP = "4318";
    otelHealth = "13133";
    tempo = "3200";
    grafana = "3001";
  };

  # Pure: the caller reads the environment, this decides the path.
  telemetryRoot =
    {
      xdgDataHome ? "",
      home ? "",
    }:
    if xdgDataHome != "" then
      "${guardedPath "XDG_DATA_HOME" xdgDataHome}/kallipai/telemetry"
    else if home != "" then
      "${guardedPath "HOME" home}/.local/share/kallipai/telemetry"
    else
      throw "obs-stack: neither XDG_DATA_HOME nor HOME is set; cannot place the telemetry data";

  # The compose service attrs plus the config texts they are built from. The
  # texts are returned as strings as well so a check can assert on them
  # without building the store paths.
  # The container ports are a parameter so a caller can evaluate the stack on
  # other ones; its default is the table above (see `defaultContainerPorts`).
  mkStack =
    {
      pkgs,
      root,
      ports,
      containerPorts ? defaultContainerPorts,
    }:
    let
      inherit (pkgs) tempo grafana;
      otelCol = pkgs.opentelemetry-collector-contrib;

      otelcolText = ''
        receivers:
          otlp:
            protocols:
              grpc:
                endpoint: 0.0.0.0:${containerPorts.otlpGRPC}
              http:
                endpoint: 0.0.0.0:${containerPorts.otlpHTTP}
        processors:
          memory_limiter:
            check_interval: 1s
            limit_mib: 256
            spike_limit_mib: 64
          batch: {}
        exporters:
          otlp:
            endpoint: tempo:${containerPorts.otlpGRPC}
            tls:
              insecure: true
        extensions:
          health_check:
            endpoint: 0.0.0.0:${containerPorts.otelHealth}
        service:
          extensions: [health_check]
          pipelines:
            traces:
              receivers: [otlp]
              processors: [memory_limiter, batch]
              exporters: [otlp]
      '';

      tempoText = ''
        server:
          http_listen_port: ${containerPorts.tempo}
        distributor:
          receivers:
            otlp:
              protocols:
                grpc:
                  endpoint: 0.0.0.0:${containerPorts.otlpGRPC}
        storage:
          trace:
            backend: local
            local:
              path: /var/tempo/blocks
            wal:
              path: /var/tempo/wal
        compactor:
          compaction:
            block_retention: ${toString maxAgeHours}h
        usage_report:
          reporting_enabled: false
      '';

      datasourceText = ''
        apiVersion: 1
        datasources:
          - name: Tempo
            type: tempo
            access: proxy
            url: http://tempo:${containerPorts.tempo}
            isDefault: true
      '';

      grafanaText = ''
        [server]
        http_port = ${containerPorts.grafana}

        # Dev-only credentials: the UI is published on loopback for one
        # operator, never on a routable interface.
        [security]
        admin_user = admin
        admin_password = admin

        [auth.anonymous]
        enabled = true
        org_role = Viewer

        [analytics]
        reporting_enabled = false
        check_for_updates = false
      '';

      texts = {
        otelcol = otelcolText;
        tempo = tempoText;
        datasource = datasourceText;
        grafana = grafanaText;
      };
    in
    {
      # The container port table this stack was built from: a caller that
      # overrides it must name endpoints from here, not from the module default.
      inherit texts containerPorts;

      services = {
        # Binaries come from the flake's nixpkgs closure (useHostStore plus
        # image.contents), so nothing is pulled from a registry.
        otelcol = {
          service = {
            useHostStore = true;
            command = [
              "${otelCol}/bin/otelcol-contrib"
              "--config=/etc/otelcol/config.yaml"
            ];
            ports = [
              "127.0.0.1:${toString ports.otlpHTTP}:${containerPorts.otlpHTTP}"
              "127.0.0.1:${toString ports.otelHealth}:${containerPorts.otelHealth}"
            ];
            volumes = [
              "${pkgs.writeText "otelcol.yaml" otelcolText}:/etc/otelcol/config.yaml:ro"
            ];
            depends_on = [ "tempo" ];
          };
          image.contents = [ otelCol ];
        };

        tempo = {
          service = {
            useHostStore = true;
            command = [
              "${tempo}/bin/tempo"
              "-config.file=/etc/tempo/config.yaml"
            ];
            ports = [ "127.0.0.1:${toString ports.tempo}:${containerPorts.tempo}" ];
            volumes = [
              "${pkgs.writeText "tempo.yaml" tempoText}:/etc/tempo/config.yaml:ro"
              "${root}/tempo:/var/tempo"
            ];
          };
          image.contents = [ tempo ];
        };

        grafana = {
          service = {
            useHostStore = true;
            command = [
              "${grafana}/bin/grafana"
              "server"
              "--homepath=${grafana}/share/grafana"
              "--config=/etc/grafana/grafana.ini"
            ];
            ports = [ "127.0.0.1:${toString ports.grafana}:${containerPorts.grafana}" ];
            volumes = [
              "${pkgs.writeText "grafana.ini" grafanaText}:/etc/grafana/grafana.ini:ro"
              "${pkgs.writeText "tempo-datasource.yaml" datasourceText}:/etc/grafana/provisioning/datasources/tempo.yaml:ro"
              "${root}/grafana:/var/lib/grafana"
            ];
            # The generated ini has no paths section, so where grafana writes is
            # set here; the rest of its settings live in grafanaText above.
            environment = {
              GF_PATHS_DATA = "/var/lib/grafana";
              GF_PATHS_LOGS = "/var/lib/grafana/logs";
              GF_PATHS_PLUGINS = "/var/lib/grafana/plugins";
              GF_PATHS_PROVISIONING = "/etc/grafana/provisioning";
            };
            depends_on = [ "tempo" ];
          };
          image.contents = [ grafana ];
        };
      };
    };
}
