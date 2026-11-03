# Static proof for the observability stack the compose files run: the texts
# nix/obs-stack.nix renders, and the wiring in compose/compose.yaml plus its two
# overrides that has to match them. Every invariant is asserted at eval time, so
# a config mounted where the image never reads it fails the check rather than
# the next `docker compose up`. The compose files are read with their comment
# lines dropped, and each wiring line must appear exactly once, so a mount
# someone commented out cannot satisfy an assertion. What only a running
# stack can prove (that the upstream entrypoint honours the path, tempo's
# real layout) is not asserted here.
{
  pkgs,
  lib,
  project,
}:
let
  stack = import ../obs-stack.nix { inherit lib; };
  # The stack package the operator mounts, imported for its file names: the
  # compose volume lines name these, so a rename here has to fail the check.
  stackPackage = import ../packages/stack.nix { inherit lib pkgs; };

  # The container side, read from the same export the stack itself uses, so
  # these assertions move with the values instead of copying them.
  inherit (stack) containerPorts;

  built = stack.mkStack { };
  # The same stack on other container ports. The numbers are arbitrary; what
  # the assertions built on it check is that nothing keeps the old ones.
  movedPorts = {
    otlpHTTP = "14318";
    otlpGRPC = "14317";
    otelHealth = "14333";
    tempo = "13200";
    grafana = "13000";
  };
  moved = stack.mkStack { containerPorts = movedPorts; };

  expect = what: cond: if cond then null else throw "obs-stack: ${what}";
  inText = name: needle: lib.hasInfix needle built.texts.${name};

  # The compose wiring, read as text. The base file carries the mounts and the
  # arguments; the two overrides carry the coordinates, and both must name the
  # same pinned reference -- a floating tag in one of them is the drift this
  # catches.
  compose = stripComments (builtins.readFile ../../compose/compose.yaml);
  composeDev = stripComments (builtins.readFile ../../compose/compose.dev.yaml);
  composeProd = stripComments (builtins.readFile ../../compose/compose.prod.yaml);
  linesOf = file: lib.splitString "\n" file;
  # Comment lines are not wiring: an assertion must not be satisfiable by a
  # line someone commented out, so the compose text is read without them.
  stripComments =
    file: lib.concatStringsSep "\n" (lib.filter (l: !(lib.hasPrefix "#" (lib.trim l))) (linesOf file));
  occurrences = needle: file: builtins.length (lib.filter (l: lib.hasInfix needle l) (linesOf file));
  # The two services that run as root, written the way compose writes them;
  # an indented Nix string keeps the quote out of an escape chain.
  rootLine = ''user: "0:0"'';
  imageRef =
    file: repo:
    let
      line = lib.findFirst (
        l: lib.hasInfix "image: ${repo}" l
      ) (throw "obs-stack: the override has no image line for ${repo}") (linesOf file);
    in
    lib.last (lib.splitString "image: " line);
  repos = [
    "otel/opentelemetry-collector-contrib:"
    "grafana/tempo:"
    "grafana/grafana:"
    "prom/prometheus:"
  ];

  # The two budgets and the unit parents are written in both languages. Reading
  # the Rust source here is what holds the copies together: a drift on one side
  # is silent at run time (the pruner spins over the wrong directory, or the two
  # tools disagree on the horizon), and this check turns it into a failed eval.
  # Finding the line is half the assertion -- a reformat that hides the value
  # from this reader fails here instead of drifting quietly.
  rustText = builtins.readFile ../../crates/kallipai-telemetry-prune/src/lib.rs;
  # A double quote spelled with an indented string, so the needles below can
  # name the marks a Rust string literal carries without escaping them.
  quote = ''"'';
  # Each suffix comes from the root derivation itself: a probe base goes in and
  # comes back off its answer, so the needle is not a third copy of the literal.
  probeBase = "/probe";
  quotedSuffix = suffix: "${quote}${suffix}${quote}";
  xdgSuffixRust = quotedSuffix (
    lib.removePrefix "${probeBase}/" (stack.telemetryRoot { xdgDataHome = probeBase; })
  );
  homeSuffixRust = quotedSuffix (
    lib.removePrefix "${probeBase}/" (stack.telemetryRoot { home = probeBase; })
  );
  rustLines = lib.splitString "\n" rustText;
  lineWith =
    prefix:
    lib.findFirst (
      line: lib.hasPrefix prefix line
    ) (throw "obs-stack: the pruner source has no line starting with '${prefix}'") rustLines;
  # The HOME arm carries no line that names its value, so this reads the arm's
  # block: the marker that opens it through the marker that closes it, both
  # spelled with their indentation. A missing marker fails the eval -- a window
  # that widened to the rest of the file would match a quote in a doc comment,
  # which is the drift these two assertions exist to catch.
  blockFrom =
    opener: closer:
    let
      openerParts = lib.splitString opener rustText;
      afterOpener =
        if builtins.length openerParts != 2 then
          throw "obs-stack: '${opener}' must appear exactly once in the pruner source"
        else
          lib.last openerParts;
      closerParts = lib.splitString closer afterOpener;
    in
    if builtins.length closerParts < 2 then
      throw "obs-stack: the pruner source has no line starting with '${closer}' after '${opener}'"
    else
      lib.head closerParts;
  numberOn = pattern: line: builtins.match pattern line;
  maxBytesRust =
    let
      found = numberOn "pub const MAX_BYTES: u64 = ([0-9]+) \\* 1024 \\* 1024 \\* 1024;" (
        lineWith "pub const MAX_BYTES: u64 = "
      );
    in
    if found == null then
      throw "obs-stack: cannot read MAX_BYTES out of the pruner source"
    else
      lib.toInt (lib.head found) * 1024 * 1024 * 1024;
  maxAgeHoursRust =
    let
      found = numberOn "pub const MAX_AGE_HOURS: u64 = ([0-9]+);" (
        lineWith "pub const MAX_AGE_HOURS: u64 = "
      );
    in
    if found == null then
      throw "obs-stack: cannot read MAX_AGE_HOURS out of the pruner source"
    else
      lib.toInt (lib.head found);
  parentsLine = lineWith "pub const UNIT_PARENTS";
  unitNames = [
    "blocks"
    "wal"
  ];
  prunerKnows = name: lib.hasInfix "tempo/${name}" parentsLine;
  tempoWrites = name: lib.hasInfix "/var/tempo/${name}" built.texts.tempo;

  assertions = [
    (expect "every container port is a string, the shape compose passes" (
      lib.all (name: builtins.isString containerPorts.${name}) (builtins.attrNames containerPorts)
    ))
    (expect "tempo's ini listens on the container port the compose mapping names" (
      inText "tempo" "http_listen_port: ${containerPorts.tempo}"
    ))
    (expect "grafana's ini listens on the container port the compose mapping names" (
      inText "grafana" "http_port = ${containerPorts.grafana}"
    ))
    (expect "the collector forwards to tempo inside the compose network" (
      inText "otelcol" "endpoint: tempo:${containerPorts.otlpGRPC}"
    ))
    (expect "the collector accepts OTLP on the port the compose mapping publishes" (
      inText "otelcol" "endpoint: 0.0.0.0:${containerPorts.otlpHTTP}"
    ))
    (expect "the collector accepts OTLP gRPC on the port it forwards to" (
      inText "otelcol" "endpoint: 0.0.0.0:${containerPorts.otlpGRPC}"
    ))
    (expect "the collector answers health probes on its container port" (
      inText "otelcol" "endpoint: 0.0.0.0:${containerPorts.otelHealth}"
    ))
    (expect "tempo accepts OTLP gRPC on the port the collector forwards to" (
      inText "tempo" "endpoint: 0.0.0.0:${containerPorts.otlpGRPC}"
    ))
    # The table is a value, not a literal: on other container ports every text
    # has to move, and no default value may stay behind. A same-value literal
    # elsewhere would keep the assertions above green.
    (expect "a moved container port moves every config text that names it" (
      lib.hasInfix "0.0.0.0:${movedPorts.otlpHTTP}" moved.texts.otelcol
      && lib.hasInfix "0.0.0.0:${movedPorts.otlpGRPC}" moved.texts.otelcol
      && lib.hasInfix "endpoint: tempo:${movedPorts.otlpGRPC}" moved.texts.otelcol
      && lib.hasInfix "endpoint: 0.0.0.0:${movedPorts.otelHealth}" moved.texts.otelcol
      && lib.hasInfix "endpoint: 0.0.0.0:${movedPorts.otlpGRPC}" moved.texts.tempo
      && lib.hasInfix "http_listen_port: ${movedPorts.tempo}" moved.texts.tempo
      && lib.hasInfix "http_port = ${movedPorts.grafana}" moved.texts.grafana
      && lib.hasInfix "url: http://tempo:${movedPorts.tempo}" moved.texts.datasource
    ))
    (expect "a moved container port leaves no default value in the texts" (
      !(lib.hasInfix "0.0.0.0:${containerPorts.otlpHTTP}" moved.texts.otelcol)
      && !(lib.hasInfix ":${containerPorts.otlpGRPC}" moved.texts.otelcol)
      && !(lib.hasInfix "http_listen_port: ${containerPorts.tempo}" moved.texts.tempo)
      && !(lib.hasInfix ":${containerPorts.otlpGRPC}" moved.texts.tempo)
    ))
    # The fixture is part of the claim: every container port is moved, and moved
    # to a different value. A moved table that kept a default, or dropped a key,
    # would leave the positive assertions above trivially true.
    (expect "the moved table covers every container port and moves it" (
      builtins.attrNames movedPorts == builtins.attrNames containerPorts
      && lib.all (name: movedPorts.${name} != containerPorts.${name}) (builtins.attrNames movedPorts)
    ))
    # The wiring: each config is mounted exactly where the upstream image reads
    # it, which is the one thing this change can silently get wrong. Each line
    # below must appear exactly once, so a second stray mount cannot hide
    # behind the first.
    (expect "the collector reads its config where the upstream image expects it" (
      occurrences "otelcol.yaml:/etc/otelcol-contrib/config.yaml:ro" compose == 1
    ))
    (expect "tempo is told to read the file that is mounted" (
      occurrences "-config.file=/etc/tempo.yaml" compose == 1
      && occurrences "tempo.yaml:/etc/tempo.yaml:ro" compose == 1
    ))
    (expect "grafana mounts the ini and the provisioning dir the image reads" (
      occurrences "/etc/grafana/grafana.ini:ro" compose == 1
      && occurrences "/etc/grafana/provisioning/datasources/tempo.yaml:ro" compose == 1
    ))
    (expect "prometheus reads its scrape config from the documented path" (
      occurrences "prometheus.yaml:/etc/prometheus/prometheus.yml:ro" compose == 1
    ))
    # The other direction: the package decides the file names, compose names the
    # sources. A rename on either side leaves a docker-created directory behind.
    # The source name is read from the volume line itself: the prefix up to `}`
    # is the stack directory expression, what follows is the file name and then
    # the container path. A name can repeat inside the container path (the
    # grafana datasource mounts tempo.yaml under /etc), so a substring cannot
    # answer this.
    (expect "compose mounts exactly the files the stack package writes" (
      let
        mounted = lib.filter (l: lib.hasInfix "KALLIPAI_STACK_DIR" l) (linesOf compose);
        sourceOf = l: lib.head (lib.splitString ":" (lib.last (lib.splitString "}" l)));
      in
      builtins.length mounted == builtins.length stackPackage.textNames
      && lib.all (
        name: builtins.length (lib.filter (l: sourceOf l == "/${name}") mounted) == 1
      ) stackPackage.textNames
    ))
    (expect "the two services writing into root-owned bind dirs run as root" (
      builtins.length (lib.filter (l: lib.trim l == rootLine) (linesOf compose)) == 2
    ))
    (expect "the collector names no command, so the image's own Cmd applies" (
      !(lib.hasInfix "\"otelcol-contrib\"" compose)
    ))
    (expect "both overrides pin the four upstream images by digest, to the same reference" (
      lib.all (
        repo:
        imageRef composeDev repo == imageRef composeProd repo
        && lib.hasInfix "@sha256:" (imageRef composeDev repo)
      ) repos
    ))
    (expect "the dev override publishes every observability port on loopback only" (
      lib.all (p: lib.hasInfix "127.0.0.1:${p}" composeDev) [
        "4318:4318"
        "13133:13133"
        "3200:3200"
        "3001:3000"
        "9090:9090"
      ]
    ))
    (expect "the stack's byte budget is the pruner's MAX_BYTES" (
      builtins.isInt maxBytesRust && maxBytesRust == stack.maxBytes
    ))
    (expect "the stack's age budget is the pruner's MAX_AGE_HOURS" (
      builtins.isInt maxAgeHoursRust && maxAgeHoursRust == stack.maxAgeHours
    ))
    (expect "tempo keeps blocks for the age the pruner enforces" (
      inText "tempo" "block_retention: ${toString maxAgeHoursRust}h"
    ))
    (expect "the pruner walks exactly the directories tempo writes" (
      lib.all (name: prunerKnows name && tempoWrites name) unitNames
    ))
    (expect "grafana ships tempo as its default datasource" (
      inText "datasource" "http://tempo:${containerPorts.tempo}" && inText "datasource" "isDefault: true"
    ))
    (expect "the root derivation prefers XDG_DATA_HOME" (
      stack.telemetryRoot {
        xdgDataHome = "/xdg";
        home = "/home/u";
      } == "/xdg/kallipai/telemetry"
    ))
    (expect "the root derivation falls back to HOME" (
      stack.telemetryRoot { home = "/home/u"; } == "/home/u/.local/share/kallipai/telemetry"
    ))
    # The suffix pair is the mirrored value with the worst failure mode: two
    # tools reading two directories leaves prune a quiet no-op over an empty
    # tree while the stack writes elsewhere. The nix side is pinned above;
    # these read the Rust side back, anchored to the arm that carries each
    # suffix (`Some(path) =>` is the XDG arm's line; the HOME suffix sits inside
    # the `None => (` arm, so that assertion reads the arm's block).
    (expect "the pruner spells the XDG suffix the root derivation appends" (
      lib.hasInfix xdgSuffixRust (lineWith "        Some(path) => ")
    ))
    (expect "the pruner spells the HOME suffix the root derivation appends" (
      lib.hasInfix homeSuffixRust (blockFrom "        None => (" "        ),")
    ))
    (expect "the root derivation refuses a relative base" (
      !(builtins.tryEval (stack.telemetryRoot { home = "relative/dir"; })).success
    ))
    (expect "the root derivation refuses '/'" (
      !(builtins.tryEval (stack.telemetryRoot { home = "/"; })).success
    ))
    (expect "the root derivation refuses a colon in the base" (
      !(builtins.tryEval (stack.telemetryRoot { xdgDataHome = "/data:ro"; })).success
    ))
    (expect "the root derivation refuses an unset environment" (
      !(builtins.tryEval (stack.telemetryRoot { })).success
    ))
  ];

  # Filtering forces every assertion above; a throw fails the eval.
  # No failure count is printed: an assertion that fails is a throw, so a run
  # with a broken invariant never reaches this line.
  passed = builtins.length (builtins.filter (x: x == null) assertions);
in
{
  "${project}-obs-stack-eval" = pkgs.runCommand "${project}-obs-stack-eval" { } ''
    mkdir -p $out
    echo ${toString passed} assertions passed > $out/assertions.txt
    cp ${pkgs.writeText "otelcol.yaml" built.texts.otelcol} $out/otelcol.yaml
    cp ${pkgs.writeText "tempo.yaml" built.texts.tempo} $out/tempo.yaml
    cp ${pkgs.writeText "grafana.ini" built.texts.grafana} $out/grafana.ini
    cp ${pkgs.writeText "tempo-datasource.yaml" built.texts.datasource} $out/tempo-datasource.yaml
  '';
}
