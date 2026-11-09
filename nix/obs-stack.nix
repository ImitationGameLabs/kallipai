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
#            receiver per tagma instance, each behind its own bearer token and
#            its own tagma.id, beside the shared receiver that in-network
#            services use -- forwarding traces to tempo and answering health
#            probes.
#   agent    otel/opentelemetry-collector-contrib as well, but the host runs
#            it, not compose: it scrapes each instance's /metrics on loopback
#            and pushes the result to that instance's intake port.
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

  # The span of loopback ports the compose files publish for the per-instance
  # intakes. The compose files carry the range as one line, so adding an
  # instance does not edit them; the numbers themselves are the deployment's,
  # given per instance in its table, and evaluation refuses a port outside this
  # span. Both overrides publish it on loopback only: the tagma and the agent
  # that push into it run on the host.
  instancePorts = {
    first = 12000;
    last = 12019;
  };
  instancePortRange = "${toString instancePorts.first}-${toString instancePorts.last}";

  # Where the render step writes one token file per instance. The path is a
  # host path, and compose mounts this directory at the same path inside the
  # collector, so a rendered text can name one string for both readers.
  # Three spellings meet on this suffix: scripts/render-intake.py writes the
  # files under it, scripts/compose-up.sh mounts the directory for the agent,
  # and nix/checks/obs-stack.nix asserts both name it, so none of the three
  # can drift alone.
  gatewayTokensDir = telemetryRoot: "${telemetryRoot}/gateway/tokens";

  # One tagma instance the intake answers for. The instance table is the
  # deployment's, read from compose/telemetry-instances.json:
  #   slug        names the files and the receiver's transform constant
  #   otlpPort    the loopback port this instance's tagma and agent push to
  #   metricsPort the loopback port this instance's tagma serves /metrics on
  #   dataRoot    the instance's data directory, where its credential is read
  #   tokenPath   where the render step wrote this instance's credential
  # A malformed table fails evaluation here rather than half-configuring the
  # collector: two instances cannot share a slug, a port or a token file, and
  # no port may repeat the shared intake's numbers.
  guardedInstances =
    instances:
    let
      field = name: i: i.${name} or (throw "obs-stack: an instance has no '${name}'");
      slugs = map (field "slug") instances;
      ports = map (field "otlpPort") instances;
      metrics = map (field "metricsPort") instances;
      duplicated = xs: builtins.length xs != builtins.length (lib.unique xs);
      repeats =
        xs: lib.unique (builtins.filter (x: builtins.length (builtins.filter (y: y == x) xs) > 1) xs);
      shared = [
        (lib.toInt containerPorts.otlpHTTP)
        (lib.toInt containerPorts.otlpGRPC)
        (lib.toInt containerPorts.otelHealth)
      ];
    in
    if duplicated slugs then
      throw "obs-stack: two instances share a slug (${lib.concatStringsSep ", " slugs})"
    else if duplicated ports then
      throw "obs-stack: two instances share an intake port (${lib.concatStringsSep ", " (map toString (repeats ports))})"
    else if duplicated metrics then
      throw "obs-stack: two instances share a management port (${lib.concatStringsSep ", " (map toString (repeats metrics))})"
    else if duplicated (map (field "tokenPath") instances) then
      throw "obs-stack: two instances share a token file"
    else if duplicated (map (field "dataRoot") instances) then
      throw "obs-stack: two instances share a data root, so one credential would open two doors"
    else if builtins.any (p: builtins.elem p shared) ports then
      throw "obs-stack: an instance reuses a shared intake port"
    else if builtins.any (p: builtins.elem p shared) metrics then
      throw "obs-stack: an instance reuses a shared port as its management port"
    else if lib.any (p: p < instancePorts.first || p > instancePorts.last) ports then
      throw "obs-stack: an instance intake port is outside ${instancePortRange}, which is what the compose files publish"
    else if lib.any (i: builtins.match "[a-z0-9][a-z0-9_-]+" (field "slug" i) == null) instances then
      throw "obs-stack: an instance slug must be lowercase letters, digits, '-' or '_'"
    else
      instances;

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
    gatewayTokensDir
    instancePorts
    instancePortRange
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
  # The container ports are a parameter so the check can rebuild the texts
  # through that argument and prove they carry no literal number.
  mkStack =
    {
      containerPorts ? defaultContainerPorts,
      # The deployment's instance table, read from
      # compose/telemetry-instances.json by whoever renders the texts. Empty
      # means no instance reaches the intake on its own port: the shared
      # receiver still serves the compose network, and no agent config exists
      # because there is nothing for it to scrape.
      instances ? [ ],
    }:
    let
      guarded = guardedInstances instances;

      # The texts are assembled line by line rather than as one indented
      # string: a per-instance block repeats the same shape, and a flat list
      # keeps every line's indentation visible where the block is added.
      lines = lib.concatStringsSep "\n";

      # One instance's intake: its own receiver on its own port, its own
      # bearer authenticator, and a transform that writes the slug over
      # whatever the client claimed, so the tenant is the door and not a
      # header the sender chose.
      receiverLines = i: [
        "  otlp/${i.slug}:"
        "    protocols:"
        "      http:"
        "        endpoint: 0.0.0.0:${toString i.otlpPort}"
        "        auth:"
        "          authenticator: bearertokenauth/${i.slug}"
      ];
      processorLines = i: [
        "  transform/${i.slug}:"
        "    error_mode: ignore"
        "    trace_statements:"
        "      - context: resource"
        "        statements:"
        "          - set(attributes[\"tagma.id\"], \"${i.slug}\")"
        "    metric_statements:"
        "      - context: resource"
        "        statements:"
        "          - set(attributes[\"tagma.id\"], \"${i.slug}\")"
      ];
      extensionLines = i: [
        "  bearertokenauth/${i.slug}:"
        "    scheme: Bearer"
        "    token: \${file:${i.tokenPath}}"
      ];
      pipelineLines = i: [
        "    traces/${i.slug}:"
        "      receivers: [otlp/${i.slug}]"
        "      processors: [memory_limiter, batch, transform/${i.slug}]"
        "      exporters: [otlp]"
        "    metrics/${i.slug}:"
        "      receivers: [otlp/${i.slug}]"
        "      processors: [memory_limiter, batch, transform/${i.slug}]"
        "      exporters: [prometheusremotewrite]"
      ];

      otelcolText =
        lines (
          [
            "receivers:"
            "  otlp:"
            "    protocols:"
            "      grpc:"
            "        endpoint: 0.0.0.0:${containerPorts.otlpGRPC}"
            "      http:"
            "        endpoint: 0.0.0.0:${containerPorts.otlpHTTP}"
          ]
          ++ lib.concatMap receiverLines guarded
          ++ [
            "processors:"
            "  memory_limiter:"
            "    check_interval: 1s"
            "    limit_mib: 256"
            "    spike_limit_mib: 64"
            "  batch: {}"
            # The shared door (the compose network, no per-instance port)
            # claims no tenant: it removes whatever tagma.id the client
            # declared, so a process that can reach it cannot land spans
            # under an instance's name. It gets no name of its own either:
            # a made-up slug would collide with a real one and read like a
            # deployment.
            "  transform/anonymous:"
            "    error_mode: ignore"
            "    trace_statements:"
            "      - context: resource"
            "        statements:"
            "          - delete_key(attributes, \"tagma.id\")"
          ]
          ++ lib.concatMap processorLines guarded
          ++ [
            "exporters:"
            "  otlp:"
            "    endpoint: tempo:${containerPorts.otlpGRPC}"
            "    tls:"
            "      insecure: true"
            "  prometheusremotewrite:"
            "    endpoint: http://prometheus:${containerPorts.prometheus}/api/v1/write"
            # One series per instance means the tenant has to be a label on
            # the series, not only on the target_info series: without this the
            # remote write keeps tagma.id on target_info alone and every
            # instance's numbers arrive looking alike.
            "    resource_to_telemetry_conversion:"
            "      enabled: true"
            "extensions:"
            "  health_check:"
            "    endpoint: 0.0.0.0:${containerPorts.otelHealth}"
          ]
          ++ lib.concatMap extensionLines guarded
          ++ [ "service:" ]
          ++ [
            (
              "  extensions: ["
              + lib.concatStringsSep ", " ([ "health_check" ] ++ map (i: "bearertokenauth/${i.slug}") guarded)
              + "]"
            )
          ]
          ++ [
            "  pipelines:"
            "    traces:"
            "      receivers: [otlp]"
            "      processors: [memory_limiter, batch, transform/anonymous]"
            "      exporters: [otlp]"
          ]
          ++ lib.concatMap pipelineLines guarded
        )
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

      # The host side: one agent for the whole host, scraping each instance's
      # /metrics on loopback and pushing it to that instance's intake port with
      # that instance's token. It reads the same token file the collector reads,
      # through the collector's own file provider, so a rotation is one file
      # write and two restarts.
      agentLines =
        lines (
          [ "receivers:" ]
          ++ lib.concatMap (i: [
            "  prometheus/${i.slug}:"
            "    config:"
            "      scrape_configs:"
            "        - job_name: ${i.slug}"
            "          scrape_interval: 15s"
            "          static_configs:"
            "            - targets: [\"127.0.0.1:${toString i.metricsPort}\"]"
          ]) guarded
          ++ [
            "processors:"
            "  batch: {}"
            "exporters:"
          ]
          ++ lib.concatMap (i: [
            "  otlphttp/${i.slug}:"
            "    endpoint: http://127.0.0.1:${toString i.otlpPort}"
            "    headers:"
            "      authorization: \"Bearer \${file:${i.tokenPath}}\""
          ]) guarded
          ++ [
            "service:"
            "  pipelines:"
          ]
          ++ lib.concatMap (i: [
            "    metrics/${i.slug}:"
            "      receivers: [prometheus/${i.slug}]"
            "      processors: [batch]"
            "      exporters: [otlphttp/${i.slug}]"
          ]) guarded
        )
        + "\n";

      # No instance, no agent: an agent config with no pipeline is a file
      # nobody can start, so the render step is told there is nothing to
      # render rather than handed an invalid one.
      agentText = if guarded == [ ] then null else agentLines;

      texts = {
        otelcol = otelcolText;
        tempo = tempoText;
        datasource = datasourceText;
        grafana = grafanaText;
        agent = agentText;
      };
    in
    {
      # The container port table this stack was built from: a caller that
      # overrides it must name endpoints from here, not from `defaultContainerPorts`.
      inherit texts containerPorts;
      # The guarded table travels too: a reader can prove a text was built
      # from the same instances it names.
      instances = guarded;
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
