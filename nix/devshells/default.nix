# Default devShell: backend toolchain only (Rust + TS + Nix).
# Entered via `nix develop` (or direnv with KALLIP_DEVSHELL unset/default).
#
# sccache (shared rustc cache across worktrees) is opt-in: set USE_SCCACHE=1
# before entering the shell to install sccache and route cargo through it.
# builtins.getEnv is impure, so a plain `nix develop` (pure eval) ignores it —
# direnv's `use flake` runs `nix print-dev-env --impure`, so the .envrc path
# picks it up automatically; ad-hoc `nix develop` needs `--impure`. The read is
# devShell-only, so package/check eval stays pure regardless.
{
  pkgs,
  lib,
  common,
}:

let
  # builtins.getEnv is impure (not tracked by the flake lock); confined to the
  # devShell so package/check eval stays pure. Truthy only on explicit opt-in.
  sccacheEnabled = builtins.getEnv "USE_SCCACHE" == "1";
in
common.craneLib.devShell (
  {
    # Extra inputs can be added here; cargo and rustc are provided by default.
    packages =
      with pkgs;
      [
        # Rust
        cargo-hakari
        rust-analyzer

        # Typescript
        deno

        # Nix
        nil
        nixfmt
        statix

        # TOML toolkit (linter, formatter)
        taplo

        # Markdown formatter (prettier pads/aligns tables, reflowing every row
        # on a one-line edit; rumdl does not). TS/Svelte/CSS formatting still
        # uses prettier from node_modules via `deno task fmt`.
        rumdl

        # Temporary workaround for copilot-cli direnv integration bug
        # See: https://github.com/github/copilot-cli/issues/731
        # TODO: Remove once the upstream issue is resolved
        bashInteractive
      ]
      # aifed is Linux-only; keep it out of the darwin devShell.
      ++ lib.optionals pkgs.stdenv.isLinux [ aifed ]
      # Shared rustc cache: cargo routes rustc through sccache (see
      # RUSTC_WRAPPER below), so compiling the same crate in multiple worktrees
      # hits the cache instead of rebuilding. Devshell-only; the crane package
      # builds run in their own sandboxed derivations and are unaffected.
      ++ lib.optional sccacheEnabled sccache;
  }
  # crane.devShell forwards extra attrs to mkShell, which exports them as env
  # vars. Keep sccache's env grouped with the binary so the shell is consistent:
  # the wrapper is only meaningful when sccache is installed.
  // lib.optionalAttrs sccacheEnabled {
    # Route cargo's rustc invocations through sccache (content-addressed cache,
    # shared across worktrees via ~/.cache/sccache). To temporarily disable in
    # a shell: `unset RUSTC_WRAPPER`.
    RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
    # sccache does not cache crates built with -C incremental (cargo's default
    # in the dev profile), so the cross-worktree cache only bites when cargo's
    # own incremental compilation is off. sccache hits replace it (a hit is a
    # fast cache read). See https://github.com/mozilla/sccache/issues/236.
    CARGO_INCREMENTAL = "0";
  }
)
