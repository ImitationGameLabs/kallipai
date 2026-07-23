# Default devShell: backend toolchain only (Rust + TS + Nix).
# Entered via `nix develop` (or direnv with KALLIP_DEVSHELL unset/default).
{
  pkgs,
  lib,
  common,
}:

common.craneLib.devShell {
  # Extra inputs can be added here; cargo and rustc are provided by default.
  packages =
    with pkgs;
    [
      # Rust
      cargo-hakari
      rust-analyzer
      # Shared rustc cache: cargo routes rustc through sccache (see RUSTC_WRAPPER
      # below), so compiling the same crate in multiple worktrees hits the cache
      # instead of rebuilding. Devshell-only; the crane package builds run in
      # their own sandboxed derivations and are unaffected.
      sccache

      # Typescript
      deno

      # Nix
      nil
      nixfmt
      statix

      # TOML toolkit (linter, formatter)
      taplo

      # Markdown formatter
      prettier

      # Temporary workaround for copilot-cli direnv integration bug
      # See: https://github.com/github/copilot-cli/issues/731
      # TODO: Remove once the upstream issue is resolved
      bashInteractive
    ]
    # aifed is Linux-only; keep it out of the darwin devShell.
    ++ lib.optionals pkgs.stdenv.isLinux [ aifed ];

  # Route cargo's rustc invocations through sccache (content-addressed cache,
  # shared across worktrees via ~/.cache/sccache). crane.devShell forwards
  # extra attrs to mkShell, which exports them as env vars. To temporarily
  # disable in a shell: `unset RUSTC_WRAPPER`.
  RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";
  # sccache does not cache crates built with -C incremental (cargo's default in
  # the dev profile), so the cross-worktree cache only bites when cargo's own
  # incremental compilation is off. sccache hits replace it (a hit is a fast
  # cache read). See https://github.com/mozilla/sccache/issues/236.
  CARGO_INCREMENTAL = "0";
}
