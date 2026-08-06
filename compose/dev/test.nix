# Dev integration-test composition: runs the workspace's `[[test]]` targets
# inside a container to confirm the sandbox and shell backends behave in the
# containerized environment the tagma ships in. One-shot -- the service exits
# with the overall verdict (`arion ps -a`, 0 = all passed).
#
# Invoke from the repo root:
#   arion -f compose/dev/test.nix up
#   arion ps -a          # exit code is the verdict (0 = all tests passed)
#   arion logs tagma
#
# Split from arion-compose.nix: the test runner shares nothing with the agora
# side (caddy/agora/lesche/agora-postgres/lesche-postgres) -- its only inputs are the pre-built
# integration-tests closure and the shared shell/CA layer from
# container-shared.nix. A flat single-purpose file here mirrors
# compose/dev/tagma.nix and lets arion-compose.nix drop the old
# KALLIP_ARION_MODE switch entirely.
{ pkgs, ... }:
let
  # Load via git+file URL (not a bare path) so getFlake applies fetchGit's VCS
  # filtering and the resolved package matches `nix build
  # .#kallip-integration-tests` bit-for-bit.
  flake = builtins.getFlake "git+file://${toString ../..}";
  integrationTests = flake.packages.x86_64-linux.kallip-integration-tests;

  shared = import ../../nix/packages/container-shared.nix { inherit pkgs; };
  inherit (shared)
    toolEnv
    cacert
    aifed
    binPath
    ;
in
{
  config = {
    project.name = "kallipai-test";

    services.tagma = {
      # The tagma's landlock/seccomp shell sandbox needs these privileges
      # (see docs/reference/container.md). No typed option for security_opt;
      # out.service is the documented escape hatch (attrsOf, merges with the
      # computed spec).
      service.capabilities.SYS_ADMIN = true;
      out.service.security_opt = [ "seccomp=unconfined" ];
      # Adds root-level /bin/sh and /usr/bin/env symlinks. The tagma and
      # agent shells don't need them (bash is resolved via PATH/toolEnv); this
      # is a convenience for `arion exec` and the iterate script below relies
      # on /bin/sh.
      image.enableRecommendedContents = true;
      image.contents = [
        integrationTests
        toolEnv
        aifed
      ]
      ++ cacert;
      service.useHostStore = true;
      # Run every pre-built [[test]] binary:
      #   - `--nocapture` surfaces scenario eprintln! diagnostics in `arion logs`;
      #   - each libtest harness exits non-zero on failure, so `arion ps -a`
      #     reports the overall verdict;
      #   - `set -e` fail-fasts across binaries (order is alphabetical: exec,
      #     then sandbox -- an early failure would mask later ones);
      #   - `[ -e ]` guards an empty glob; `found` refuses to silently pass
      #     when no test binary is present.
      # restart = "no" so the exited container isn't restarted.
      # Caveat: --nocapture assumes each binary is a libtest harness; a future
      # [[test]] with `harness = false` would reject the flag and fail fast.
      service.command = [
        "/bin/sh"
        "-c"
        ''
          set -e
          found=0
          for t in /integration-tests/*; do
            [ -e "$t" ] || continue
            found=1
            echo "=== integration test: $t ==="
            "$t" --nocapture
          done
          [ "$found" = 1 ] || { echo "no integration tests found"; exit 1; }
        ''
      ];
      service.restart = "no";
      # /testdata holds the sandbox scenarios' home/data/workspace scratch
      # dirs. It MUST be outside libsandbox's baseline-writable set (/tmp,
      # /var/tmp, $TMPDIR) so write-denial assertions stay honest; a dedicated
      # tmpfs is the simplest such path. Same escape hatch as security_opt.
      out.service.tmpfs = [ "/testdata:rw,size=64m" ];
      service.environment = {
        PATH = "${integrationTests}/bin:${binPath}";
        # Explicit agent-bin dir for resolve_bin -- current_exe() resolves
        # the buildEnv symlink into a sub-store path, not the shared bin/.
        KALLIP_BIN_DIR = "${integrationTests}/bin";
        KALLIP_TESTDATA_DIR = "/testdata";
        HOME = "/var/lib/kallip";
        RUST_LOG = "info";
      };
    };
  };
}
