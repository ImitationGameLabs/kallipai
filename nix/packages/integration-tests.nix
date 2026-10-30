# Pre-build the test binaries the integration container runs, and check that
# every test target in the workspace has been classified.
#
# The integration tests are kept `[[test]]` targets (not `[[bin]]`) so their
# dev-deps (wiremock, tempfile, serial_test, ...) stay out of the shipped
# tagma's regular dep tree. crane's buildPackage emits `[[bin]]` targets, not
# test binaries, so we drive `cargo test --no-run` ourselves: it compiles every
# test harness binary without running it. `--message-format=json` yields each
# artifact's exact path (the `deps/<name>-<hash>` layout is not stable enough
# to glob).
#
# What the image *ships* is an explicit allowlist (`shippedTests`). Every other
# target is named in `ignoredTests` with the reason it stays out, and both
# tables are keyed by `<package>:<target>` -- a bare name is not unique, since
# two crates here both carry a `lifecycle` target. A name-keyed table would let
# a third same-named target pass every check below while shipping nothing and
# explaining nothing.
#
# Each reason starts with `environment:` (the container cannot run it: a
# daemon, a postgres, a host socket) or `policy:` (it could run here, and we
# do not ship it).
#
# The build asserts the workspace's `<package>:<target>` set equals the
# allowlist plus the ignore table, so a new target fails the build until it is
# classified, and an entry that no longer matches a target fails it too (a
# stale classification is how a silent shrinkage hides). Each shipped name
# must resolve to exactly one target, which is what makes the bare-name
# allowlist safe.
{
  common,
  pkgs,
  workspace,
}:
let
  inherit (common)
    craneLib
    commonArgs
    cargoArtifacts
    ;

  inherit (pkgs) lib;

  # Suites the container runs, by target name. Each must name exactly one
  # target in the workspace; the classification check below proves that.
  shippedTests = [
    "exec"
    "sandbox"
  ];

  # Everything else in the workspace, keyed `<package>:<target>`, with the
  # reason it stays out. The reason is for the next person who sees the target
  # in `cargo test --no-run` output and wonders why the container skips it.
  ignoredTests = {
    "kallip:file_e2e" = "environment: needs a live files daemon and a host-writable tree";
    "kallipai-adk:dispatch_background_cell" =
      "policy: exercises background dispatch across real threads; the crate's unit tests cover it";
    "kallipai-common:log_bridge" =
      "policy: asserts the tracing bridge through a real subscriber and log files";
    "kallipai-daemon:env_harvest" =
      "environment: starts a real daemon process and reads its environment";
    "kallipai-daemon:lifecycle" =
      "environment: spawns a real tagma binary through KALLIPAI_BIN_DIR and drives its sockets";
    "kallipai-files:api_tests" = "environment: needs a live files daemon and its postgres";
    "kallipai-files:ingest_tests" = "environment: needs a live files daemon and its postgres";
    "kallipai-files:list_tests" = "environment: needs a live files daemon and its postgres";
    "kallipai-files:range_tests" = "environment: needs a live files daemon and its postgres";
    "kallipai-instances:instances_roundtrip" =
      "environment: starts an instances daemon and talks to its socket in a tempdir";
    "kallipai-model-gateway:metrics_boot" =
      "environment: needs a postgres to boot model-gateway and scrape its metrics face";
    "kallipai-tagma:env_governance_boot" =
      "environment: boots tagma on a host filesystem with env-governance fixtures";
    "kallipai-tagma:roots_isolation" =
      "environment: needs the crate's test-support feature and host filesystem isolation across boots";
    "kallipai-tagma:workspace_relocation" =
      "environment: needs the crate's test-support feature and a relocatable host workspace";
    "kallipai-task:lifecycle" =
      "policy: drives the task store against a temporary sqlite database; the task crate's own suite";
    "kallipai-telemetry-prune:prune" =
      "policy: drives the prune CLI over a temporary telemetry tree; a dev-loop probe";
    "kallipai-testkit:timeout_msg_probe" = "policy: asserts testkit timeout messages; a dev-loop probe";
  };

  ignoredKeys = builtins.attrNames ignoredTests;

  # A target in both tables would pass the union check below while its binary
  # silently carried a "does not ship here" reason.
  shippedAsIgnored = lib.filter (
    key: lib.elem (lib.last (lib.splitString ":" key)) shippedTests
  ) ignoredKeys;

  integrationTestsBin = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;

      # Build the test binaries only (not run). `--release` matches crane's
      # default CARGO_PROFILE=release (used by both buildDepsOnly and
      # buildPackage, so cargoArtifacts is release-built) -- the dep cache is
      # reused. The feature is package-scoped: `sandbox-test` lives on
      # `kallipai-tagma` and gates the `sandbox` target; `exec` needs none.
      #
      # doNotPostBuildInstallCargoBinaries: the buildPhase is a custom `cargo
      # test --no-run` (not `cargo build`), so crane's auto-install-from-build-log
      # hook has nothing to consume -- the installPhase below handles it.
      doNotPostBuildInstallCargoBinaries = true;
      buildPhase = ''
        runHook preBuild

        # Classification check first: a target added without a decision fails
        # here, before the compile, rather than after several minutes of
        # building. `cargo metadata` is the one source that lists every
        # declared target, including any `required-features` keeps from
        # compiling today; the compiler-artifact stream below is the only
        # place with executable paths.
        cargo metadata --offline --no-deps --format-version 1 \
          | ${pkgs.jq}/bin/jq -r '.packages[] as $p | $p.targets[] | select(((.kind // []) | index("test")) != null) | "\($p.name):\(.name)"' \
          | sort -u > "$NIX_BUILD_TOP/workspace-tests"

        # This block runs in the build shell, not a subshell, so its `exit 1`
        # aborts the build directly.
        {
          printf '%s\n' ${lib.escapeShellArgs ignoredKeys}
          for shipped in ${lib.escapeShellArgs shippedTests}; do
            matches=$(grep -c ":''${shipped}$" "$NIX_BUILD_TOP/workspace-tests" || true)
            if [ "$matches" -ne 1 ]; then
              echo "integration-tests: shipped name '$shipped' matches $matches workspace targets; a shipped name must name exactly one" >&2
              exit 1
            fi
            grep ":''${shipped}$" "$NIX_BUILD_TOP/workspace-tests"
          done
        } | sort -u > "$NIX_BUILD_TOP/classified-tests"

        if ! diff -u "$NIX_BUILD_TOP/classified-tests" "$NIX_BUILD_TOP/workspace-tests"; then
          echo "integration-tests: workspace test targets drifted from shippedTests union ignoredTests" >&2
          echo "integration-tests: classify the new target in nix/packages/integration-tests.nix" >&2
          exit 1
        fi

        cargo test --no-run --release --locked \
          --features kallipai-tagma/sandbox-test \
          --message-format=json \
          | ${pkgs.jq}/bin/jq -r 'select(.reason=="compiler-artifact" and ((.target.kind // []) | any(. == "test")) and .executable != null) | "\(.target.name)\t\(.executable)"' \
          > "$NIX_BUILD_TOP/test-bins"
        test -s "$NIX_BUILD_TOP/test-bins" || {
          echo "integration-tests: no test binaries found in cargo JSON output" >&2
          exit 1
        }

        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p "$out/integration-tests"

        # One pass, one mechanism: the same `case` decides what is copied and
        # counts the rows, so a same-named target in two crates shows up as two
        # copies of one name rather than as a silent overwrite.
        shipped_rows=0
        while IFS=$'\t' read -r name exe; do
          case "$name" in
            ${lib.concatStringsSep "|" shippedTests}) ;;
            *)
              # Compiled (the JSON stream covers the whole workspace) but not
              # shipped; see ignoredTests for the reason.
              continue
              ;;
          esac
          shipped_rows=$((shipped_rows + 1))
          cp "$exe" "$out/integration-tests/$name"
          echo "integration-tests: installed $name"
        done < "$NIX_BUILD_TOP/test-bins"

        expected_rows=${toString (lib.length shippedTests)}
        if [ "$shipped_rows" -ne "$expected_rows" ]; then
          echo "integration-tests: expected $expected_rows shipped binaries, found $shipped_rows (same-named [[test]] in two crates?)" >&2
          exit 1
        fi

        # What landed must be exactly the allowlist: a suite that failed to
        # compile, or a filter that silently stopped matching, shows up here.
        (cd "$out/integration-tests" && ls -1 | sort) > "$NIX_BUILD_TOP/installed-tests"
        printf '%s\n' ${lib.escapeShellArgs (lib.sort builtins.lessThan shippedTests)} \
          | sort -u > "$NIX_BUILD_TOP/shipped-tests"
        if ! diff -u "$NIX_BUILD_TOP/shipped-tests" "$NIX_BUILD_TOP/installed-tests"; then
          echo "integration-tests: installed suites differ from shippedTests" >&2
          exit 1
        fi

        runHook postInstall
      '';

      doCheck = false;
    }
  );
in
assert lib.assertMsg (
  shippedTests != [ ]
) "integration-tests: shippedTests is empty; the install filter would not parse";
assert lib.assertMsg (
  shippedAsIgnored == [ ]
) "integration-tests: listed in both tables: ${lib.concatStringsSep ", " shippedAsIgnored}";
(pkgs.buildEnv {
  name = "kallipai-integration-tests";
  paths = [
    workspace
    integrationTestsBin
  ];
  pathsToLink = [
    "/bin"
    "/integration-tests"
  ];
})
// {
  # The image that runs these suites compares its own /integration-tests
  # against this list, so the expectation travels with the artifact instead of
  # being re-derived from the directory it is meant to check.
  passthru = { inherit shippedTests; };
}
