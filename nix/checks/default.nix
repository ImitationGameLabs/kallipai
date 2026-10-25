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

  project = "kallipai";
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
      # crane cargoDoc already runs --locked --no-deps by default;
      # no override needed. Warning denial comes from the repo-root
      # .cargo/config.toml (build.warnings), which the crane filter keeps
      # through its .toml suffix match.
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
      # Warning denial comes from the repo-root .cargo/config.toml
      # (build.warnings), which the crane filter keeps through its .toml
      # suffix match.
      cargoExtraArgs = "--locked --all-features";
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
  # 15:40:17Z; reason = kallipai-audit hits three RUSTSEC advisories
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

  # The check's definition lives in ./module-eval.nix; the import
  # returns an attrset of check names, and this entry selects the
  # check by the dynamically built name.
  "${project}-nixos-module-eval" =
    (import ./module-eval.nix {
      inherit
        pkgs
        lib
        aifed
        project
        ;
    })."${project}-nixos-module-eval";

  # The compose files are Nix modules that only `arion up` evaluates, and
  # arion is not a flake input -- so nothing here would notice a file that no
  # longer parses. Parsing needs no arion and no evaluation, and it catches the
  # class of defect that reads fine but cannot be loaded (`services = x;` next
  # to `services.<name>.y = z;` is a parse error, not a merge). The dummy store
  # keeps the parser from touching a store this build must not write.
  "${project}-compose-parse" =
    pkgs.runCommand "${project}-compose-parse"
      {
        nativeBuildInputs = [ pkgs.nix ];
        files = [
          ../../compose/dev/polis.nix
          ../../compose/dev/tagma.nix
          ../../compose/dev/files.nix
          ../../compose/dev/test.nix
          ../../compose/prod/polis.nix
          ../../compose/prod/tagma.nix
          ../../arion-compose.nix
          ../../arion-pkgs.nix
        ];
      }
      ''
        for file in $files; do
          nix-instantiate --store dummy:// --parse "$file" > /dev/null
        done
        mkdir -p $out
        echo "every compose file parses" > $out/parse.txt
      '';

  # The local observability stack's expression (nix/obs-stack.nix): arion is
  # not an input, so the stack is a pure function and this check is the
  # only place its shape is proven without starting anything.
  "${project}-obs-stack-eval" =
    (import ./obs-stack.nix {
      inherit
        pkgs
        lib
        project
        ;
    })."${project}-obs-stack-eval";
}
