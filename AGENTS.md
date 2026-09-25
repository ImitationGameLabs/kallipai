# AGENTS.md

AI Agent working guide. This document provides code structure and decision rules for AI agents.

## Naming

The project name is `kallipai` (literally `kallip` + `ai`). The brand has one written form, `KallipAI`; use it on every human-facing surface: the README and doc H1, and prose where the brand appears. Use `kallipai` for every technical surface: crate names, package names, binaries, Rust module paths, env var prefixes (`KALLIPAI_*`), on-disk paths, container paths/volumes, Nix attrs, Cargo/flake `description` strings, User-Agent. Two grandfathered exceptions keep their names: the headless CLI `kallip` and the operator CLI `kallipctl`. When a sentence is mixed, prefer `kallipai`. For the name's origin and the rules behind it, see [naming.md](docs/en/naming.md).

## Comments

Code and test comments are self-contained: do not reference out-of-repo documents, do not carry schedule codenames, and point references only at objects resolvable inside the repository. References to published external standards (RFC, W3C) are exempt.

## Directory Structure

```text
.
├── flake.nix                  # Flake entry point
├── crates/                    # Rust workspace members
│   ├── kallipai-common/   # Shared types and command parsing
│   ├── kallipai-adk/  # Agent development kit: agent context, policy, tool dispatch (tagma-only)
│   ├── kallipai-shell/    # Reusable shell/session tools for LLM applications
│   ├── kallip/           # Headless CLI for agent (tagma client)
│   ├── kallipai-tagma/   # HTTP API server hosting multiple agent instances
│   ├── kallipai-run/      # Agent runner for scripting and benchmarking
│   ├── kallipai-client/   # Tagma client library
│   ├── time/             # Timer/scheduling subsystem: cron daemon that fires schedules and injects them into conversations, plus its wire types, HTTP client, and management CLI
│   ├── platform/        # Public-internet relay subsystem (servers + wire types + clients + admin + E2E crypto)
│       ├── kallipai-archeion/     # Control-plane relay server
│       ├── kallipai-lesche/      # Data-plane relay server
│       ├── kallipai-archeion-common/ # Wire types for the relay and its clients
│       ├── kallipai-lesche-common/ # Wire types for the relay data plane
│       ├── kallipai-e2ee/        # End-to-end encryption primitives (Ed25519 device key, X3DH KEX, AEAD)
│       ├── kallipai-archeion-client/ # Archeion relay HTTP client (enroll + admin surface)
│       ├── kallipai-lesche-client/ # Lesche data-plane relay HTTP client
│       ├── kallipai-admin/        # Headless archeion admin CLI (sk-admin HTTP client)
│       ├── kallipai-instances/  # Local instance management service (web API + static UI over the daemon)
│   └── daemon/          # Local instance management subsystem (UDS daemon + wire types + clients + spawn helper + CLI)
│       ├── kallipai-daemon/     # Stateless, directory-driven manager for local instances (UDS control socket)
│       ├── kallipai-daemon-common/ # Wire types for the daemon protocol
│       ├── kallipai-daemon-client/ # UDS client library for the daemon
│       ├── kallipai-daemon-spawn/  # Detached spawn helper that launches instance processes
│       └── kallipctl/            # Management CLI for the daemon
├── packages/                  # JS/TS workspace (Deno-first; see below)
│   ├── kallipai-common/         # Transport-agnostic shared types + SSE parser
│   ├── kallipai-client/         # Direct tagma HTTP+SSE client (offline path)
│   ├── kallipai-archeion-client/   # Archeion control-plane + WebAuthn client
│   ├── kallipai-lesche-client/  # Lesche data-plane client (online E2EE path)
│   ├── kallipai-ui/             # Shared SvelteKit UI library
│   ├── kallipai-web/            # SvelteKit web app
│   └── kallipai-app/            # Tauri (Android) app shell
├── docs/                      # Project documentation
│   └── en/                   # Documentation tree the site renders
│       ├── architecture.md       # System architecture, tagma design, policy
│       ├── context-management.md # Agentic context management design
│       ├── development/      # Contributor guides
│       │   ├── setup.md      # Workspace bring-up and verification
│       │   ├── frontend.md   # Deno-first frontend toolchain
│       │   └── i18n.md       # Message catalogs (i18n)
│       └── reference/        # Reference documentation
│           ├── auth.md       # Authentication and authorization
│           ├── tagma-api.md  # HTTP API endpoints
│           ├── env/          # Environment variable reference
│           ├── kallip.md     # `kallip` headless CLI for agent
│           └── kallipai-run.md # `kallipai-run` agent runner for scripting
└── nix/
      ├── common.nix           # Core config (crate paths, dependencies)
      ├── checks.nix           # CI checks
      └── packages/
            └── tarball.nix    # Release tarball builder
```

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
