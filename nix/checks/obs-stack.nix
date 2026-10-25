# Static proof for nix/obs-stack.nix: the stack expression is evaluated here
# without arion (which is not a flake input), and every invariant the dev
# compose relies on is asserted at eval time -- a broken shape fails the check
# rather than the next `arion up`. What only a running stack can prove (the
# arion option typing, the containers' uid, the real tempo layout) is not
# asserted here.
{
  pkgs,
  lib,
  project,
}:
let
  stack = import ../obs-stack.nix { inherit lib; };

  # A root the shape assertions can name literally; nothing is created.
  root = "/home/operator/.local/share/kallipai/telemetry";
  # The real defaults, not a copy of them: a second literal here would be the
  # second place to forget, and every port assertion below would then be
  # checking this file against itself. These are strings, the shape
  # compose/dev/polis.nix passes after envOrDefault.
  ports = stack.defaults;
  # The container side, read from the same export the stack itself uses, so
  # these assertions move with the values instead of copying them.
  inherit (stack) containerPorts;
  built = stack.mkStack { inherit pkgs root ports; };
  # The same stack on other container ports. The numbers are arbitrary; what
  # the assertions built on it check is that nothing keeps the old ones.
  movedPorts = {
    otlpHTTP = "14318";
    otlpGRPC = "14317";
    otelHealth = "14333";
    tempo = "13200";
    grafana = "13000";
  };
  moved = stack.mkStack {
    inherit pkgs root ports;
    containerPorts = movedPorts;
  };
  # The dev tagma's host port default, read out of its source: the same move as
  # reading the pruner's constants below, and the only way to pin both sides of
  # "the UI does not land on the tagma's port".
  tagmaPort =
    let
      line =
        lib.findFirst (candidate: lib.hasInfix "else tagmaPortRaw" candidate)
          (throw "obs-stack: compose/dev/tagma.nix has no tagma port default")
          (lib.splitString "\n" (builtins.readFile ../../compose/dev/tagma.nix));
      found = builtins.match ".*then \"([0-9]+)\" else tagmaPortRaw.*" line;
    in
    if found == null then
      throw "obs-stack: cannot read the tagma port default out of compose/dev/tagma.nix"
    else
      lib.head found;
  inherit (built) services;
  names = builtins.attrNames services;
  get = name: attr: services.${name}.service.${attr};
  allTrue = builtins.all (x: x);

  expect = what: cond: if cond then null else throw "obs-stack: ${what}";
  rootOf = config: path: "${config}/${path}";
  inVolumes = name: entry: lib.elem entry (get name "volumes");

  contents = name: services.${name}.image.contents or [ ];
  headOf = name: lib.head (get name "command");

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
    (expect "the stack is exactly the collector, tempo and grafana" (
      names == [
        "grafana"
        "otelcol"
        "tempo"
      ]
    ))
    (expect "every service runs a host-store binary, never a pulled image" (
      allTrue (map (n: get n "useHostStore" && contents n != [ ]) names)
    ))
    (expect "every service's command starts its own image contents" (
      allTrue (map (n: lib.any (item: lib.hasPrefix "${item}" (headOf n)) (contents n)) names)
    ))
    (expect "no service joins the host network" (
      allTrue (map (n: (services.${n}.service.network_mode or null) == null) names)
    ))
    (expect "every published port is loopback-only" (
      allTrue (lib.concatMap (n: map (p: lib.hasPrefix "127.0.0.1:" p) (get n "ports")) names)
    ))
    (expect "the collector publishes OTLP and health only, on the caller's host ports" (
      get "otelcol" "ports" == [
        "127.0.0.1:${ports.otlpHTTP}:${containerPorts.otlpHTTP}"
        "127.0.0.1:${ports.otelHealth}:${containerPorts.otelHealth}"
      ]
    ))
    (expect "tempo publishes the caller's host port onto its own container port" (
      get "tempo" "ports" == [ "127.0.0.1:${ports.tempo}:${containerPorts.tempo}" ]
    ))
    (expect "grafana publishes the caller's host port onto its own container port" (
      get "grafana" "ports" == [ "127.0.0.1:${ports.grafana}:${containerPorts.grafana}" ]
    ))
    (expect "tempo's ini listens on the container port its publish names" (
      lib.hasInfix "http_listen_port: ${containerPorts.tempo}" built.texts.tempo
    ))
    (expect "grafana's ini listens on the container port its publish names" (
      lib.hasInfix "http_port = ${containerPorts.grafana}" built.texts.grafana
    ))
    (expect "every default host port is a string, the shape compose passes" (
      lib.all (name: builtins.isString stack.defaults.${name}) (builtins.attrNames stack.defaults)
    ))
    (expect "every container port is a string, the shape compose passes" (
      lib.all (name: builtins.isString containerPorts.${name}) (builtins.attrNames containerPorts)
    ))
    # Both sides of the UI port question: the stack's own host port and the
    # one the dev tagma binds, read out of that file rather than assumed to be
    # a literal.
    (expect "the UI's default host port is not the one the dev tagma binds" (
      stack.defaults.grafana != tagmaPort
    ))
    (expect "both stateful services bind their state under the telemetry root" (
      inVolumes "tempo" (rootOf root "tempo:/var/tempo")
      && inVolumes "grafana" (rootOf root "grafana:/var/lib/grafana")
    ))
    (expect "the collector forwards to tempo inside the compose network" (
      lib.hasInfix "endpoint: tempo:${containerPorts.otlpGRPC}" built.texts.otelcol
    ))
    (expect "the collector accepts OTLP on the published port" (
      lib.hasInfix "endpoint: 0.0.0.0:${containerPorts.otlpHTTP}" built.texts.otelcol
    ))
    (expect "the collector accepts OTLP gRPC on the port it forwards to" (
      lib.hasInfix "endpoint: 0.0.0.0:${containerPorts.otlpGRPC}" built.texts.otelcol
    ))
    (expect "the collector answers health probes on its container port" (
      lib.hasInfix "endpoint: 0.0.0.0:${containerPorts.otelHealth}" built.texts.otelcol
    ))
    (expect "tempo accepts OTLP gRPC on the port the collector forwards to" (
      lib.hasInfix "endpoint: 0.0.0.0:${containerPorts.otlpGRPC}" built.texts.tempo
    ))
    # The table is a value, not a literal: on other container ports every text
    # and publish mapping has to move, and no default value may stay behind.
    # A same-value literal elsewhere would keep the assertions above green.
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
    (expect "a moved container port moves every publish mapping" (
      moved.services.otelcol.service.ports == [
        "127.0.0.1:${ports.otlpHTTP}:${movedPorts.otlpHTTP}"
        "127.0.0.1:${ports.otelHealth}:${movedPorts.otelHealth}"
      ]
      && moved.services.tempo.service.ports == [ "127.0.0.1:${ports.tempo}:${movedPorts.tempo}" ]
      && moved.services.grafana.service.ports == [ "127.0.0.1:${ports.grafana}:${movedPorts.grafana}" ]
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
    (expect "the stack's byte budget is the pruner's MAX_BYTES" (
      builtins.isInt maxBytesRust && maxBytesRust == stack.maxBytes
    ))
    (expect "the stack's age budget is the pruner's MAX_AGE_HOURS" (
      builtins.isInt maxAgeHoursRust && maxAgeHoursRust == stack.maxAgeHours
    ))
    (expect "tempo keeps blocks for the age the pruner enforces" (
      lib.hasInfix "block_retention: ${toString maxAgeHoursRust}h" built.texts.tempo
    ))
    (expect "the pruner walks exactly the directories tempo writes" (
      lib.all (name: prunerKnows name && tempoWrites name) unitNames
    ))
    (expect "grafana ships tempo as its default datasource" (
      lib.hasInfix "http://tempo:${containerPorts.tempo}" built.texts.datasource
      && lib.hasInfix "isDefault: true" built.texts.datasource
    ))
    (expect "grafana keeps its state under the bound data dir" (
      lib.all (k: lib.hasPrefix "/var/lib/grafana" services.grafana.service.environment.${k}) [
        "GF_PATHS_DATA"
        "GF_PATHS_LOGS"
        "GF_PATHS_PLUGINS"
      ]
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
    # the `None => (` arm, so that assertion reads the arm's block). A needle matched
    # anywhere in the file would stay green on a quote in a doc comment after
    # the value had moved.
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
