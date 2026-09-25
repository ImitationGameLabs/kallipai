# AGENTS.md

AI Agent working guide. This document provides code structure and decision rules for AI agents.

## Naming

The project name is `kallipai` (literally `kallip` + `ai`). The brand has one written form, `KallipAI`; use it on every human-facing surface: the README and doc H1, and prose where the brand appears. Use `kallipai` for every technical surface: crate names, package names, binaries, Rust module paths, env var prefixes (`KALLIPAI_*`), on-disk paths, container paths/volumes, Nix attrs, Cargo/flake `description` strings, User-Agent. Two grandfathered exceptions keep their names: the headless CLI `kallip` and the operator CLI `kallipctl`. When a sentence is mixed, prefer `kallipai`. For the name's origin and the rules behind it, see [naming.md](docs/en/naming.md).

## Comments

Code and test comments are self-contained: do not reference out-of-repo documents, do not carry schedule codenames, and point references only at objects resolvable inside the repository. References to published external standards (RFC, W3C) are exempt.

## Directory Structure

```text
.
├── flake.nix            # Flake entry point
├── crates/              # Rust workspace members
├── packages/            # JS/TS workspace (Deno-first; see below)
├── docs/                # Project documentation (en/, zh-cn/)
├── skills/              # Agent skill library
├── harbor-integration/  # Harbor test-framework integration
├── compose/             # Container compositions (dev and prod)
└── nix/                 # Nix packaging and checks
```

`crates/` holds the core crates plus three subsystem homes: `platform/`
(public-internet relay), `daemon/` (local instance management), and
`time/` (timer/scheduling). Each subsystem directory contains its own
crates; elsewhere one directory is one crate, and directory names match
crate names. The README table describes the core crates.

## Frontend development

When working on anything under `packages/`, read [frontend.md](docs/en/development/frontend.md) first. It defines the Deno-first toolchain: every action (dev, build, check, fmt, lint, installing deps) goes through `deno task`. Do not drop down to `npm`/`npx`/`pnpm`/`yarn` or hand-invoke `node_modules/.bin/*`; if a workflow is missing, add a `scripts` entry and call it via `deno task`.

## Dependency Management

When adding dependencies to any crate:

1. Look up the latest version: `cargo search <crate-name> --registry crates-io`
2. Add to `[workspace.dependencies]` in root `Cargo.toml`
3. Reference in crate's `Cargo.toml` with `workspace = true`

Example:

```toml
# Root Cargo.toml
[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }

# crates/my-app/Cargo.toml
[dependencies]
serde = { workspace = true }
```

## Verification Checklist

After modifying Nix files:

- `nixfmt <nix file>` - Format single file
- `nixfmt $(find nix/ -name "*.nix") flake.nix` - Format all Nix files at once
- `statix check flake.nix && statix check nix/` - Static analysis (run from project root)

After modifying TOML files:

- `taplo fmt <toml file>` - Format specific file (never use bare `taplo fmt` — it ignores .gitignore and formats everything)

After modifying Markdown files:

- `deno task fmt:md <markdown file>` - Format with rumdl (run individually for each modified file). Markdown uses rumdl, not prettier -- rumdl does not align table columns, so a one-row edit never reflows the rest of the table.

After modifying Rust code:

- `cargo fmt` - Format check
- `cargo clippy --workspace --all-targets --all-features` - Lint check
- `cargo test --workspace --all-targets --all-features` - Run tests
- `cargo doc --workspace --all-features --no-deps` - Build docs
- `cargo check`, clippy, test, and doc deny warnings via `.cargo/config.toml` (build.warnings = deny). To relax for one invocation, prefix with `CARGO_BUILD_WARNINGS=allow`.
