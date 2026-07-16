# AGENTS.md

AI Agent working guide. This document provides code structure and decision rules for AI agents.

## Naming

The project brand is `kallipai` (literally `kallip` + `ai`); the technical stem is `kallip`. Use the `Kallip AI` / `kallipai` brand forms only for human-facing surfaces (README and doc H1, prose where the brand reads better). Use `kallip` for every technical surface: crate names, binaries, Rust module paths, env var prefixes (`KALLIP_*`), on-disk paths, container paths/volumes, Nix attrs, Cargo/flake `description` strings, User-Agent, Harbor `name()`. When a sentence is mixed, prefer `kallip`. For the history and rationale behind these names, see [naming.md](docs/naming.md).

## Directory Structure

```
.
├── flake.nix                  # Flake entry point
├── crates/                    # Rust workspace members
│   ├── kallip-common/    # Shared types and command parsing
│   ├── kallip-runtime/   # Agent runtime: agent context, policy, tool dispatch (daemon-only)
│   ├── kallip-shell/     # Reusable shell/session tools for LLM applications
│   ├── kallip/           # Headless CLI for agent (daemon client)
│   ├── kallip-tui/       # Interactive TUI client
│   ├── kallip-daemon/    # HTTP API server hosting multiple agent instances
│   ├── kallip-run/       # Agent runner for scripting and benchmarking
│   └── kallip-client/    # Daemon client library
├── docs/                      # Project documentation
│   ├── architecture.md       # System architecture, daemon design, policy
│   ├── context-management.md # Agentic context management design
│   ├── agent-wizards/        # Step-by-step guides for common agent tasks
│   └── reference/            # Reference documentation
│       ├── auth.md           # Authentication and authorization
│       ├── daemon-api.md     # HTTP API endpoints
│       ├── env.md            # Environment variable reference
│       ├── kallip.md     # `kallip` headless CLI for agent
│       └── kallip-run.md # `kallip-run` agent runner for scripting
└── nix/
      ├── common.nix           # Core config (crate paths, dependencies)
      ├── checks.nix           # CI checks
      └── packages/
            └── tarball.nix    # Release tarball builder
```

## Frontend development

When working on anything under `packages/` (the JS/TS workspace: `kallip-web`, `kallip-ui`, `kallip-app`, `kallip-client`, `kallip-common`, `kallip-agora-client`), read [frontend-development.md](docs/frontend-development.md) first. It defines the Deno-first toolchain: every action (dev, build, check, fmt, lint, installing deps) goes through `deno task`. Do not drop down to `npm`/`npx`/`pnpm`/`yarn` or hand-invoke `node_modules/.bin/*`; if a workflow is missing, add a `scripts` entry and call it via `deno task`.

## Common Tasks

For adding workspace members, see [add-workspace-member.md](docs/agent-wizards/add-workspace-member.md).

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

- `deno task fmt:file <markdown file>` - Format specific file (run individually for each modified file)

After modifying Rust code:

- `cargo fmt` - Format check
- `cargo clippy --workspace --all-targets --all-features` - Lint check
- `cargo test --workspace --all-targets --all-features` - Run tests
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps` - Build docs and fail on rustdoc warnings
