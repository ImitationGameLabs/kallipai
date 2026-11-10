# The config texts for the observability stack: one set the compose
# containers read, one the NixOS module writes for the host's own tempo.
# A pure function -- no services.*, no pkgs -- so every consumer
# evaluates it without a container runtime: nix/packages/stack.nix
# renders the compose texts, nix/checks/obs-stack.nix asserts their
# ports and mounts, nix/checks/obs-nixos.nix verifies the host text,
# and nix/nixos-modules.nix feeds it to the tempo unit.
#
# What runs
#   otelcol  otel/opentelemetry-collector-contrib: the OTLP intake -- one
#            listener behind one platform token, forwarding traces to tempo
#            and metrics to prometheus, beside the health probe. The labels on
#            what arrives are the sender's own: nothing here reads a tenant or
#            writes one.
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
  # The metrics budget the compose recorder is started with: the module's
  # option default reads this, and the check compares the compose flag to it.
  metricsRetention = "30d";
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
    prometheus = "9090";
  };

  # The same table under a second name: a formal argument cannot default to a
  # variable of its own name, and `mkStack`'s container port parameter has to
  # default to this one.
  defaultContainerPorts = containerPorts;

  # Where the collector container reads the platform credential: compose mounts
  # the deployment's token file there, and the text names this path and nothing
  # else. One door, one token, one path.
  defaultIntakeTokenPath = "/etc/kallipai/intake-token";

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
  inherit
    maxBytes
    maxAgeHours
    metricsRetention
    containerPorts
    defaultIntakeTokenPath
    ;

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
  # Both the ports and the token path are parameters so the check can rebuild
  # the texts through them and prove they carry no literal of their own.
  mkStack =
    {
      containerPorts ? defaultContainerPorts,
      # Where, inside the collector container, the one platform credential
      # sits. Compose mounts the deployment's file there; the text names the
      # mount point and never a token.
      intakeTokenPath ? defaultIntakeTokenPath,
    }:
    let
      # The text is one flat list, one line per entry, so every indentation
      # stays visible where it is written.
      lines = lib.concatStringsSep "\n";

      otelcolText =
        lines [
          "receivers:"
          "  otlp:"
          "    protocols:"
          "      grpc:"
          "        endpoint: 0.0.0.0:${containerPorts.otlpGRPC}"
          "        auth:"
          "          authenticator: bearertokenauth"
          "      http:"
          "        endpoint: 0.0.0.0:${containerPorts.otlpHTTP}"
          "        auth:"
          "          authenticator: bearertokenauth"
          "processors:"
          "  memory_limiter:"
          "    check_interval: 1s"
          "    limit_mib: 256"
          "    spike_limit_mib: 64"
          "  batch: {}"
          "exporters:"
          "  otlp:"
          "    endpoint: tempo:${containerPorts.otlpGRPC}"
          "    tls:"
          "      insecure: true"
          "  prometheusremotewrite:"
          "    endpoint: http://prometheus:${containerPorts.prometheus}/api/v1/write"
          # Without this conversion the sender's resource attributes ride
          # target_info alone and two tagmas' series arrive looking alike; it
          # puts them on every series.
          "    resource_to_telemetry_conversion:"
          "      enabled: true"
          "extensions:"
          "  health_check:"
          "    endpoint: 0.0.0.0:${containerPorts.otelHealth}"
          # The one platform credential: a file the deployment owns, read by the
          # collector's file provider, so a rotation is one file write and one
          # restart.
          "  bearertokenauth:"
          "    scheme: Bearer"
          "    token: \${file:${intakeTokenPath}}"
          "service:"
          "  extensions: [health_check, bearertokenauth]"
          "  pipelines:"
          "    traces:"
          "      receivers: [otlp]"
          "      processors: [memory_limiter, batch]"
          "      exporters: [otlp]"
          "    metrics:"
          "      receivers: [otlp]"
          "      processors: [memory_limiter, batch]"
          "      exporters: [prometheusremotewrite]"
        ]
        + "\n";

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
          - name: Prometheus
            type: prometheus
            access: proxy
            url: http://prometheus:${containerPorts.prometheus}
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
  # The trace store's config for a host that runs its own stack (the NixOS
  # module), beside the container text above. The two are separate on
  # purpose: the container image pins tempo 2.9.x, whose retention knob is
  # compactor.compaction.block_retention, while nixpkgs ships tempo 3.x,
  # where the same budget lives under backend_scheduler.work.prune_age.
  # One text handed to the other major fails configuration validation
  # outright. Merge them if the compose image and the nixpkgs package land
  # on one major.
  hostTempoText =
    {
      listenAddress,
      httpPort,
      otlpHttpPort,
      otlpGrpcPort,
      storageRoot,
      retentionHours,
    }:
    ''
      server:
        http_listen_address: ${listenAddress}
        http_listen_port: ${toString httpPort}
        grpc_listen_address: ${listenAddress}
      distributor:
        receivers:
          otlp:
            protocols:
              grpc:
                endpoint: ${listenAddress}:${toString otlpGrpcPort}
              http:
                endpoint: ${listenAddress}:${toString otlpHttpPort}
      storage:
        trace:
          backend: local
          local:
            path: ${guardedPath "storageRoot" storageRoot}/blocks
          wal:
            path: ${guardedPath "storageRoot" storageRoot}/wal
      backend_scheduler:
        work:
          prune_age: ${toString retentionHours}h
      usage_report:
        reporting_enabled: false
      # The live store keeps its own WAL and a shutdown marker: both default
      # under /var/tempo, which the unit's account does not own. Keeping them
      # under the same root as the blocks puts every written file inside the
      # state directory systemd hands the unit.
      live_store:
        shutdown_marker_dir: ${guardedPath "storageRoot" storageRoot}/live-store/shutdown-marker
        wal:
          path: ${guardedPath "storageRoot" storageRoot}/live-store/traces
    '';

}
