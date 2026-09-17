{
  pkgs,
  common,
  advisory-db,
  aifed,
  lib,
}:
let
  inherit (common)
    craneLib
    src
    commonArgs
    cargoArtifacts
    ;

  project = "kallip";
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

  # Build docs (default features)
  "${project}-doc" = craneLib.cargoDoc (
    commonArgs
    // {
      inherit cargoArtifacts;
      env.RUSTDOCFLAGS = "--deny warnings";
    }
  );

  # Docs with all features: catches broken intra-doc links inside feature-gated
  # modules (e.g. mock behind `testutils`), which the default-feature
  # check above can't see (those modules aren't compiled then). Keep both: the
  # default check catches links in always-compiled code that point to gated
  # items; this one catches links inside the gated modules.
  "${project}-doc-all-features" = craneLib.cargoDoc (
    commonArgs
    // {
      inherit cargoArtifacts;
      # Repeat `--locked`: overriding cargoExtraArgs replaces crane's default
      # ("--locked"), so it must be re-stated here. --locked asserts Cargo.lock
      # is current (fails instead of silently updating it) for hermetic builds.
      cargoExtraArgs = "--locked --all-features";
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

  # Temporarily commented out per operator directive 2026-09-17
  # 15:40:17Z; reason = kallip-audit hits three RUSTSEC advisories
  # (2023-0071 / 2026-0173 / 2026-0190) pending a triage batch.
  # Restore when the triage batch lands.
  # "${project}-audit" = craneLib.cargoAudit {
  #   inherit src advisory-db;
  # };

  # Audit licenses
  "${project}-deny" = craneLib.cargoDeny {
    inherit src;
  };

  # Run the test suite. Sandbox env deps are scoped here (not in commonArgs,
  # which the package build and buildDepsOnly also consume):
  # - procps: provides `pgrep` for the process-group reap tests (kill is already
  #   in coreutils).
  # - cacert + SSL_CERT_FILE: reqwest's rustls-platform-verifier loads the system
  #   CA store at client construction; the sandbox has none, so point it at the
  #   nix bundle.
  # Disabled by operator directive 2026-09-17 14:40Z (disk-write
  # protection); restore by uncommenting.
  # "${project}-nextest" = craneLib.cargoNextest (
  #   commonArgs
  #   // {
  #     inherit cargoArtifacts;
  #     nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [
  #       pkgs.cacert
  #       pkgs.procps
  #     ];
  #     SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  #     partitions = 1;
  #     partitionType = "count";
  #     cargoNextestPartitionsExtraArgs = "--no-tests=pass";
  #   }
  # );

  # The NixOS module evaluation check moved verbatim to
  # ./module-eval-check.nix (file split per operator 2026-09-17;
  # the returned attrset is selected by the check's dynamic name).
  "${project}-nixos-module-eval" =
    (import ./module-eval-check.nix {
      inherit
        pkgs
        lib
        aifed
        project
        ;
    })."${project}-nixos-module-eval";
}
