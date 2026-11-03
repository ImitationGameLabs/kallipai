# The config texts for the observability stack the compose files run: a
# collector, a trace store and a UI. A pure function -- no services.*, no
# pkgs -- so both consumers evaluate it without a container runtime:
# nix/packages/stack.nix renders the texts the compose services mount, and
# nix/checks/obs-stack.nix asserts the ports, paths and mounts they need.
#
# What runs
#   otelcol  otel/opentelemetry-collector-contrib: the OTLP receiver (http
#            published to the host, grpc inside the compose network),
#            forwarding traces to tempo and answering health probes.
#   tempo    grafana/tempo: the trace store: query API published on loopback,
#            OTLP received inside the network only; blocks retained for the
#            age budget below.
#   grafana  grafana/grafana: the UI, published on loopback, anonymous viewer,
#            tempo pre-provisioned as the default datasource so a trace id
#            pastes straight into Explore.
#
# The images come from upstream registries, pinned by digest in the compose
# files; this file describes no container, only the text each one reads.
#
# Which port carries which role lives in the containerPorts table below, not
# in the texts: every text names a container port from there, and the compose
# files carry the host side of the mapping.
#
# Where the data lands (outside the project, so `docker compose down -v`
# cannot eat it)
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

  # Every container port the config texts name. The host side lives in the
  # compose files: a deployment that moves a published port does not have to
  # touch a text, because none of them names a host port.
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

  # The config texts, plus the container port table they were built from, so a
  # caller (and the check) can see which numbers went in. The texts are plain
  # strings: a check asserts on them without building a store path, and the
  # nix/packages/stack.nix writes them into the directory compose mounts.
  # The container ports are a parameter so the check can rebuild the texts
  # through that argument and prove they carry no literal number.
  mkStack =
    {
      containerPorts ? defaultContainerPorts,
    }:
    let

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
      # overrides it must name endpoints from here, not from `defaultContainerPorts`.
      inherit texts containerPorts;
    };
}
