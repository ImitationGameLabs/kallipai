# The observability stack on a booted host, end to end: the four units come
# up, the recorder answers its readiness probe and reaches its own target,
# the UI serves the two provisioned datasources, and a span posted to the
# OTLP/HTTP receiver is read back out of the trace store by its trace id.
# Everything the eval-time checks cannot see -- a unit that cannot start, a
# password file the file provider cannot read, a receiver that accepts a
# payload the query side never returns -- fails here.
{
  pkgs,
  lib,
  aifed,
  project,
}:
let
  # The module's package argument, stubbed: this test runs the observability
  # units only, and the platform services stay switched off.
  stubPackage =
    name:
    pkgs.runCommand "${name}-stub" { } ''
      mkdir -p $out/bin
      for binname in ${lib.concatStringsSep " " stubBins.${name}}; do
        echo '${name}' > $out/bin/$binname
        chmod +x $out/bin/$binname
      done
    '';
  stubBins = {
    kallipai-host = [
      "kallipai-daemon"
      "kallipctl"
    ];
    workspace = [
      "kallipai-archeion"
      "kallipai-lesche"
      "kallipai-keryx"
      "kallipai-files"
      "kallipai-instances"
      "kallipai-model-gateway"
      "kallipai-admin"
    ];
  };
  stubPackages = {
    ${pkgs.stdenv.hostPlatform.system} = builtins.mapAttrs (name: _: stubPackage name) stubBins // {
      kallipai-web-dist = pkgs.runCommand "kallipai-web-dist-stub" { } ''
        mkdir $out
        echo bundle-shell > $out/config.js
        echo bundle-page > $out/index.html
      '';
    };
  };
  # The span the test posts, and the id it reads back.
  traceId = "0af7651916cd43dd8448eb211c80319c";
  spanId = "b7ad6b7169203331";
  spanName = "vm-probe";
in
pkgs.testers.runNixOSTest {
  name = "${project}-nixos-observability";

  nodes.machine =
    { ... }:
    {
      imports = [
        (import ../nixos-modules.nix {
          packages = stubPackages;
          aifedOverlay = aifed.overlays.default;
        })
      ];
      system.stateVersion = "26.11";
      boot.loader.grub.device = "/dev/null";
      # The platform services are not what this test proves: only the stack.
      services.kallipai.daemon.enable = false;
      services.kallipai.observability.enable = true;
      virtualisation.memorySize = 2048;
    };

  testScript = ''
    start_all()

    # The four units, and the four listeners.
    machine.wait_for_unit("kallipai-tempo.service")
    machine.wait_for_unit("prometheus.service")
    machine.wait_for_unit("grafana.service")
    machine.wait_for_unit("prometheus-node-exporter.service")
    for port in [3200, 4318, 4317, 9090, 3000, 9100]:
        machine.wait_for_open_port(port)

    # Nothing in this stack listens beyond loopback: tempo's gRPC face
    # carries no authentication, so an all-interfaces bind would be
    # reachable from anywhere the host firewall lets through.
    listening = [
        line.split()[3]
        for line in machine.succeed("${pkgs.iproute2}/bin/ss -ltnH").splitlines()
        if line.split()
    ]
    beyond = [
        addr
        for addr in listening
        if not addr.startswith("127.") and not addr.startswith("[::1]:")
    ]
    assert not beyond, "listening beyond loopback: %s" % beyond
    assert "127.0.0.1:9095" in listening, listening

    # The recorder accepts work and reaches the host target it was given.
    machine.succeed("curl -fsS http://127.0.0.1:9090/-/ready")
    machine.wait_until_succeeds(
        "curl -fsS http://127.0.0.1:9090/api/v1/targets"
        " | grep -q '\"job\":\"kallipai-node\"'"
        " && curl -fsS http://127.0.0.1:9090/api/v1/targets"
        " | grep -q '\"health\":\"up\"'",
        timeout=120,
    )

    # The trace store's own /metrics is a target too: a stopped tempo shows
    # up as a down job instead of merely no traces.
    machine.wait_until_succeeds(
        "curl -fsS http://127.0.0.1:9090/api/v1/targets | grep -q '\"job\":\"tempo\"'",
        timeout=120,
    )
    import json
    import time

    def tempo_target():
        active = json.loads(
            machine.succeed("curl -fsS http://127.0.0.1:9090/api/v1/targets")
        )["data"]["activeTargets"]
        found = [t for t in active if t["labels"].get("job") == "tempo"]
        assert found, [t["labels"].get("job") for t in active]
        return found[0]

    deadline = time.time() + 120
    while tempo_target()["health"] != "up":
        assert time.time() < deadline, tempo_target()
        time.sleep(2)

    # The UI answers, and the password the unit generated on first start is
    # the one the file provider hands it: both datasources read through it.
    machine.succeed("curl -fsS http://127.0.0.1:3000/api/health | grep -q ok")
    admin = machine.succeed("cat /var/lib/grafana/admin-password").strip()
    machine.succeed(
        "curl -fsS -u admin:%s http://127.0.0.1:3000/api/datasources"
        " | grep -q http://127.0.0.1:9090" % admin
    )
    machine.succeed(
        "curl -fsS -u admin:%s http://127.0.0.1:3000/api/datasources"
        " | grep -q http://127.0.0.1:3200" % admin
    )

    # The trace store runs the text the unit names, on the retention budget
    # the module documents.
    machine.succeed(
        "cfg=$(systemctl show -p ExecStart --value kallipai-tempo.service"
        " | grep -o -- '-config.file=[^ ]*' | cut -d= -f2);"
        " grep -q 'prune_age: 72h' \"$cfg\""
    )

    # A span in through the OTLP/HTTP receiver, out through the query API:
    # the two halves of the store are one pipeline.
    payload = (
        '{"resourceSpans":[{"resource":{"attributes":['
        '{"key":"service.name","value":{"stringValue":"vm-probe"}}]},'
        '"scopeSpans":[{"scope":{"name":"vm"},"spans":[{'
        '"traceId":"${traceId}","spanId":"${spanId}","name":"${spanName}",'
        '"kind":1,"startTimeUnixNano":"1700000000000000000",'
        '"endTimeUnixNano":"1700000001000000000"}]}]}]}'
    )
    machine.succeed(
        "curl -fsS -X POST http://127.0.0.1:4318/v1/traces"
        " -H 'Content-Type: application/json' -d '%s'" % payload
    )
    machine.wait_until_succeeds(
        "curl -fsS http://127.0.0.1:3200/api/traces/${traceId} | grep -q ${spanName}",
        timeout=120,
    )
  '';
}
