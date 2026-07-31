# DevShell concerns shared by devShells.default and devShells.tauri, factored
# out so the two stay in sync for repo-wide tooling and the opt-in sccache
# scheme. What is NOT shared — the craneLib toolchain (different cross
# targets), backend-only deps (cargo-hakari, aifed), and the tauri-only
# Android toolchain + env — stays in each shell.
#
# sccache (shared rustc cache across worktrees) is opt-in: set USE_SCCACHE=1
# before entering the shell to install sccache and route cargo through it.
# builtins.getEnv is impure, so a plain `nix develop` (pure eval) ignores it —
# direnv's `use flake` runs `nix print-dev-env --impure`, so the .envrc path
# picks it up automatically; ad-hoc `nix develop` needs `--impure`. The read is
# devShell-only (this module is imported only by the two devShells, never by
# the package/check graph), so package/check eval stays pure regardless.
{ pkgs, lib }:

let
  # Truthy only on explicit opt-in; see the file header for the purity rationale.
  sccacheEnabled = builtins.getEnv "USE_SCCACHE" == "1";
in
{
  # Repo-wide dev tooling used by both shells.
  tooling = with pkgs; [
    # Rust LSP
    rust-analyzer

    # Typescript (drives deno task ...)
    deno

    # Nix: LSP + formatter + linter
    nil
    nixfmt
    statix

    # TOML toolkit (linter, formatter)
    taplo

    # Markdown formatter (prettier pads/aligns tables, reflowing every row on
    # a one-line edit; rumdl does not). TS/Svelte/CSS formatting still uses
    # prettier from node_modules via `deno task fmt`.
    rumdl

    # Local CA + leaf cert generation for the Caddy-fronted dev topology (see
    # the mkcert step in docs/development.md); used by arion-compose.nix +
    # compose/dev/Caddyfile.dev.
    mkcert

    # Temporary workaround for copilot-cli direnv integration bug
    # See: https://github.com/github/copilot-cli/issues/731
    # TODO: Remove once the upstream issue is resolved
    bashInteractive
  ];

  # Apply the opt-in sccache scheme to a devShell *argument* attrset: when
  # USE_SCCACHE=1, install sccache and route cargo's rustc through it. Must
  # wrap the argument passed to craneLib.devShell, NOT the derivation it
  # returns — mkShell strips `packages` from its result (via
  # excludeDrvArgNames), so merging after the fact would be inert.
  #
  # Bounded merge: only `packages` (appended to) and the two cargo env vars
  # below are ever set — everything else passes through untouched.
  withSccache =
    attrs:
    attrs
    // lib.optionalAttrs sccacheEnabled {
      # Shared rustc cache: cargo routes rustc through sccache (see
      # RUSTC_WRAPPER below), so compiling the same crate in multiple worktrees
      # hits the cache instead of rebuilding. Devshell-only; the crane package
      # builds run in their own sandboxed derivations and are unaffected.
      packages = (attrs.packages or [ ]) ++ [ pkgs.sccache ];
      # Route cargo's rustc invocations through sccache (content-addressed
      # cache, shared across worktrees via ~/.cache/sccache). To temporarily
      # disable in a shell: `unset RUSTC_WRAPPER`.
      RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
      # sccache does not cache crates built with -C incremental (cargo's
      # default in the dev profile), so the cross-worktree cache only bites
      # when cargo's own incremental compilation is off. sccache hits replace
      # it (a hit is a fast cache read). See
      # https://github.com/mozilla/sccache/issues/236.
      CARGO_INCREMENTAL = "0";
    };
}
