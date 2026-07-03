# Pre-build every `[[test]]` binary for in-container execution.
#
# The integration tests are kept `[[test]]` targets (not `[[bin]]`) so their
# dev-deps (wiremock, tempfile, serial_test, ...) stay out of the shipped
# daemon's regular dep tree. crane's buildPackage emits `[[bin]]` targets, not
# test binaries, so we drive `cargo test --no-run` ourselves: it compiles every
# test harness binary without running it. `--message-format=json` yields each
# artifact's exact path (the `deps/<name>-<hash>` layout is not stable enough
# to glob), which we copy to `$out/integration-tests/<name>`.
#
# This is generic: any `[[test]]` added anywhere in the workspace is picked up
# automatically. Today the workspace has two -- `sandbox` (gated by
# just-agent-daemon's `sandbox-test` feature) and `exec` (just-agent-shell).
#
# The agent binaries (`just-agent-daemon`, `just-agent-run`, `just-agent`) come
# from the shared `workspace` derivation; `buildEnv` merges them into `bin/`
# while the test binaries live under a separate `integration-tests/` so the
# container can iterate them independently of the agent bins.
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

  integrationTestsBin = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;

      # Build the test binaries only (not run). `--release` matches crane's
      # default CARGO_PROFILE=release (used by both buildDepsOnly and
      # buildPackage, so cargoArtifacts is release-built) -- the dep cache is
      # reused. The feature is package-scoped: `sandbox-test` lives on
      # `just-agent-daemon` and gates the `sandbox` target; `exec` needs none.
      #
      # doNotPostBuildInstallCargoBinaries: the buildPhase is a custom `cargo
      # test --no-run` (not `cargo build`), so crane's auto-install-from-build-log
      # hook has nothing to consume -- the installPhase below handles it.
      doNotPostBuildInstallCargoBinaries = true;
      buildPhase = ''
        runHook preBuild
        cargo test --no-run --release --locked \
          --features just-agent-daemon/sandbox-test \
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
        # Refuse same-named [[test]] across crates (cargo permits it); without
        # this, `cp` would silently overwrite and drop a suite. Logging each
        # name also makes a silent shrinkage (e.g. the sandbox-test feature line
        # going missing) visible in the build log.
        while IFS=$'\t' read -r name exe; do
          if [ -e "$out/integration-tests/$name" ]; then
            echo "integration-tests: duplicate test name '$name' -- same-named [[test]] in multiple crates?" >&2
            exit 1
          fi
          cp "$exe" "$out/integration-tests/$name"
          echo "integration-tests: installed $name"
        done < "$NIX_BUILD_TOP/test-bins"
        runHook postInstall
      '';

      doCheck = false;
    }
  );
in
pkgs.buildEnv {
  name = "just-agent-integration-tests";
  paths = [
    workspace
    integrationTestsBin
  ];
  pathsToLink = [
    "/bin"
    "/integration-tests"
  ];
}
