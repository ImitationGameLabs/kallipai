{
  pkgs,
  common,
  integrationTests,
  shippedTests,
}:
let
  inherit (common) gitVersion;
  inherit (pkgs) lib;

  shared = import ../container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    cacert
    aifed
    ;

  # What the container sees as `/`: the pre-built suites under
  # `/integration-tests`, the agent binaries on `/bin` (the sandbox harness
  # re-executes `kallipai-run` and `kallip` by name and cannot derive their
  # path from `current_exe()`), and the runner that drives the suites.
  # `/bin/sh` and `/usr/bin/env` are the two paths a shebang and an
  # `env` lookup need; everything they point at lives in the store the
  # compose file mounts read-only.
  runner = pkgs.writeShellScript "integration-tests-runner" ''
    set -e
    # The expectation comes from the allowlist (see the manifest line in the
    # builder), so this comparison catches an assembly that dropped or added an
    # entry, which the build's own installed-set check cannot see (it runs on
    # the derivation, not on the image).
    # An empty directory is not success: the manifest comparison below fails on
    # it (empty listing against a non-empty expectation).
    listed="$(ls /integration-tests | sort)"
    if [ "$listed" != "$(sort /integration-tests-manifest)" ]; then
      echo "the image does not carry the shipped suite set"
      echo "expected:"; sort /integration-tests-manifest
      echo "found:"; printf '%s\n' "$listed"
      exit 1
    fi
    for t in /integration-tests/*; do
      [ -e "$t" ] || continue
      echo "=== integration test: $t ==="
      "$t" --nocapture
    done
    echo "=== integration tests: all suites passed ==="
  '';

  root = pkgs.runCommand "kallipai-integration-tests-root" { } ''
    mkdir -p $out/bin $out/usr/bin
    # /tmp must exist and be world-writable: the exec suite cds into it, and
    # the sandbox's baseline-writable set names it. An image that only carries
    # the store symlinks has no /tmp of its own. This one is the fallback --
    # compose.test.yaml mounts a size-limited tmpfs at the same path, and that
    # mount is what a real run gets.
    mkdir -p $out/tmp
    chmod 1777 $out/tmp
    # The compose file sets HOME here; an image whose HOME does not exist
    # makes every tool that writes a cache fail on the first touch.
    mkdir -p $out/var/lib/kallipai
    ln -s ${integrationTests}/integration-tests $out/integration-tests
    for b in ${integrationTests}/bin/* ${toolEnv}/bin/* ${aifed}/bin/*; do
      ln -s "$b" "$out/bin/$(basename "$b")"
    done
    # /bin/sh comes with the toolset (bashInteractive ships it); /usr/bin/env
    # is added because a `#!/usr/bin/env bash` shebang is common enough that
    # its absence would look like a missing tool.
    ln -s ${pkgs.coreutils}/bin/env $out/usr/bin/env
    cp ${runner} $out/integration-tests-runner
    # The expectation comes from the allowlist in nix/packages/integration-tests.nix,
    # not from listing the directory it checks: a derivation that shipped a
    # stray or a missing suite must not be able to redefine "expected".
    printf '%s\n' ${lib.escapeShellArgs (lib.sort builtins.lessThan shippedTests)} | sort -u > $out/integration-tests-manifest
  '';
in
# The image the integration-test compose file runs: the suites, the agent
# binaries they re-execute, and the runner. It carries symlinks, not the
# closure (`includeStorePaths = false`): the compose file mounts the host
# `/nix/store` read-only, so copying several GB into layers would duplicate
# what every layer could just point at.
pkgs.dockerTools.streamLayeredImage {
  name = "kallipai-integration-tests";
  tag = gitVersion;
  includeStorePaths = false;
  contents = [ root ] ++ cacert;
  config = {
    # PATH and KALLIPAI_BIN_DIR are declared here and nowhere else: the image
    # is the contract for what the suites find on disk. A compose file that
    # repeated them would be a second place to keep in step.
    Env = [
      "PATH=/bin"
      "KALLIPAI_BIN_DIR=/bin"
    ];
  };
}
