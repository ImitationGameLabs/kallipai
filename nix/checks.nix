{
  pkgs,
  common,
  advisory-db,
}:
let
  inherit (common)
    craneLib
    src
    commonArgs
    cargoArtifacts
    ;

  project = "just-agent";
in
{
  # Run clippy (and deny all warnings) on the workspace source
  "${project}-clippy" = craneLib.cargoClippy (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";
    }
  );

  # Build docs
  "${project}-doc" = craneLib.cargoDoc (
    commonArgs
    // {
      inherit cargoArtifacts;
      env.RUSTDOCFLAGS = "--deny warnings";
    }
  );

  # Check formatting
  "${project}-fmt" = craneLib.cargoFmt {
    inherit src;
  };

  # TOML formatting
  "${project}-toml-fmt" = craneLib.taploFmt {
    src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
  };

  # Audit dependencies for security issues
  "${project}-audit" = craneLib.cargoAudit {
    inherit src advisory-db;
  };

  # Audit licenses
  "${project}-deny" = craneLib.cargoDeny {
    inherit src;
  };

  # TODO: Re-enable once integration tests are separated from unit tests.
  # The Nix sandbox lacks CA certificates, so reqwest fails to build HTTPS
  # clients at construction time. Even with cacert added, integration tests
  # need network access and API keys which are unavailable in the sandbox.
  #
  # "${project}-nextest" = craneLib.cargoNextest (
  #   commonArgs
  #   // {
  #     inherit cargoArtifacts;
  #     partitions = 1;
  #     partitionType = "count";
  #     cargoNextestPartitionsExtraArgs = "--no-tests=pass";
  #   }
  # );
}
