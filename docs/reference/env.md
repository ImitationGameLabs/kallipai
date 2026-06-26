# Environment variable reference

All configuration is done through environment variables. Copy `.env.example` to `.env` and fill in the required values. If you use `direnv`, it loads `.env` automatically via `.envrc`.

## LLM Provider

These variables select and configure the LLM backend. They are **required** when no [model profiles](#model-profiles) config file is present.

| Variable                          | Required    | Default          | Description                                                                                               |
| --------------------------------- | ----------- | ---------------- | --------------------------------------------------------------------------------------------------------- |
| `JUST_LLM_PROVIDER`               | **yes**     | —                | LLM backend. Supported values: `deepseek`, `openai-compatible`.                                           |
| `JUST_LLM_MODEL`                  | **yes**     | —                | Model identifier passed to the provider (e.g. `deepseek-v4-flash`, `glm-5.1`).                            |
| `JUST_LLM_DEEPSEEK_API_KEY`       | conditional | —                | API key for the DeepSeek provider. Required when `JUST_LLM_PROVIDER=deepseek`.                            |
| `JUST_LLM_DEEPSEEK_BASE_URL`      | no          | DeepSeek default | Override the default DeepSeek API endpoint.                                                               |
| `JUST_LLM_OPENAI_COMPAT_API_KEY`  | conditional | —                | API key for the OpenAI-compatible provider. Required when `JUST_LLM_PROVIDER=openai-compatible`.          |
| `JUST_LLM_OPENAI_COMPAT_BASE_URL` | conditional | `""`             | Override the default OpenAI-compatible API endpoint. Required when `JUST_LLM_PROVIDER=openai-compatible`. |

## Model Profiles

A profile binds a model to an endpoint and its declared capabilities (`max_context_window`), grouped into capability tiers. With a profiles config file, the daemon loads multiple provider/model combinations, each profile declaring its own `max_context_window`.

| Variable                   | Required | Default                                     | Description                                                                                                                         |
| -------------------------- | -------- | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_PROFILES_FILE` | no       | `$XDG_CONFIG_HOME/just-agent/profiles.toml` | Path to a TOML profiles config. Absent (and the default path missing) → the implicit single profile is built from `JUST_LLM_*` env. |

Without a config file (the default for benchmark/scripting via Harbor and `just-agent-run`), a single implicit profile is derived from `JUST_LLM_*` env, and its `max_context_window` is derived from `JUST_AGENT_CONTEXT_WINDOW_TOKENS` (default `128000`).

Example `profiles.toml`:

```toml
[endpoints.deepseek-primary]
family = "deepseek"
api_key = "${JUST_LLM_DEEPSEEK_API_KEY}" # env-var indirection keeps secrets out of the file

[endpoints.openrouter]
family = "openai-compatible"
api_key = "${OPENROUTER_API_KEY}"
base_url = "https://openrouter.ai/api/v1"

[[tiers]] # capability rank 0 (highest) — selected first
  [[tiers.profiles]]
  id = "deepseek-v4-pro"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-pro"
  max_context_window = 500000

[[tiers]]
  [[tiers.profiles]]
  id = "deepseek-v4-flash"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-flash"
  max_context_window = 128000
```

- `family` must be one of `deepseek`, `openai-compatible`.
- `${VAR}` in `api_key` / `base_url` is expanded from the process environment.
- The config file should be `chmod 600` (the daemon warns if group/other-readable, since it may hold API keys).

### Tier selection

Tiers are purely positional — each agent resolves `tiers[depth]`, where `depth` derives from
delegation level: root agents (depth 0) resolve to `tiers[0]` (conventionally the
highest-capability tier — order your tiers by capability), and each level of subagent
delegation moves one tier down, clamped to the last tier. There is no name and no explicit
override; treat the tier list as append-only / truncate-tail (reordering or removing a middle
tier rebinds agents silently).

The selected tier's first profile is the active model; the remaining profiles form a
within-tier failover chain. When the active profile fails terminally (HTTP 401/403/404, or
transient retries exhausted), the runner advances to the next profile in the tier and retries
the same turn; a request-level failure (400/422) errors the round instead. The active profile
index sticks for the agent's lifetime and resets to 0 on restore. No tier binding is persisted
— it is re-derived from depth on every spawn/restore.

On advance, the context window tracks the new profile's declared `max_context_window` (within-tier
windows may differ — placing models with different windows in one tier is supported). If the
carried context now exceeds the new (possibly smaller) window, the runner compacts it before
retrying, so the turn survives the switch. A candidate whose window would violate a budget
invariant is skipped _before_ the advance (so the agent never sends an oversized request to a
smaller-window model); if no feasible candidate remains, the chain is reported
`allCandidatesInfeasible` (tune `SUMMARY_MAX_TOKENS` / `PINNED_BUDGET_RATIO` or raise the window). The _active_ profile's window (not a failover candidate) is validated at spawn — a window that violates a budget invariant rejects the spawn outright (fail-fast) rather than silently falling back.

The retry budget is **per-endpoint**, not per-profile: rate limits are endpoint-scoped, so two
profiles sharing one endpoint share one budget. A profile's transient retries accumulate within
`retry_timeout` **across rounds** — this is intentional rate-limit backpressure (a persistently
failing endpoint gets fewer retries, forcing failover or a round error), and it matches the
pre-failover agent-wide behavior for the active profile. The index only advances forward, so a
failed-over-from endpoint's accumulated budget never re-bites.

Edge cases: an agent whose depth exceeds the tier count is clamped to the last
(lowest-capability) tier with a warning. With a two-tier config, every subagent level maps
onto `tiers[1]`.

Source: [`crates/just-agent-runtime/src/profile/`](../../crates/just-agent-runtime/src/profile).

## Agent Core

Runtime tuning parameters. All are optional with sensible defaults.

| Variable                                 | Default                         | Constraints                                            | Description                                                                                                                                                                                                                                                                                                                                                                                           |
| ---------------------------------------- | ------------------------------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_SYSTEM_PROMPT`               | Built-in prompt                 | —                                                      | System prompt injected into every LLM session. See `DEFAULT_SYSTEM_PROMPT` in `config.rs` for the built-in text.                                                                                                                                                                                                                                                                                      |
| `JUST_AGENT_MAX_TOOL_ROUNDS`             | _(unlimited)_                   | > 0                                                    | Maximum tool-call rounds per agent. Defaults to unlimited — the daemon-wide token budget is the primary safety net. Set this to enforce a hard round limit independent of token consumption (e.g. for testing or cost control).                                                                                                                                                                       |
| `JUST_AGENT_WORKSPACE_ROOT`              | Current directory               | —                                                      | Root directory for agent workspace. Can also be set via CLI `--workspace-root`. CLI flag takes precedence.                                                                                                                                                                                                                                                                                            |
| `JUST_AGENT_CONTEXT_WINDOW_TOKENS`       | `128000`                        | > 0                                                    | Context window size in tokens for the implicit env profile (no [profiles config](#model-profiles)) — it becomes that profile's `max_context_window`. With a config file, each profile declares its own `max_context_window` instead.                                                                                                                                                                  |
| `JUST_AGENT_OUTPUT_RESERVE_TOKENS`       | `8192`                          | < `CONTEXT_WINDOW_TOKENS`                              | Tokens reserved for model output within the context window.                                                                                                                                                                                                                                                                                                                                           |
| `JUST_AGENT_SUMMARY_MAX_TOKENS`          | `1200`                          | > 0, ≤ pinned budget                                   | Maximum tokens for compacted (summarized) context. Must fit within the pinned budget (effective budget × pinned budget ratio).                                                                                                                                                                                                                                                                        |
| `JUST_AGENT_TOOL_TIMEOUT_SECS`           | `120`                           | —                                                      | Timeout in seconds for individual tool executions.                                                                                                                                                                                                                                                                                                                                                    |
| `JUST_AGENT_PINNED_BUDGET_RATIO`         | `0.25`                          | 0.0–1.0 (exclusive)                                    | Fraction of effective budget allocated to pinned context items.                                                                                                                                                                                                                                                                                                                                       |
| `JUST_AGENT_CONTEXT_THRESHOLDS`          | `50,60,70,80`                   | Comma-separated `1`–`99`, sorted ascending, ≥ 2 values | Context usage thresholds (percentage). The last value triggers auto-compact; preceding values are warnings.                                                                                                                                                                                                                                                                                           |
| `JUST_AGENT_MAX_RETRIES`                 | `3`                             | —                                                      | Maximum retries for LLM API calls.                                                                                                                                                                                                                                                                                                                                                                    |
| `JUST_AGENT_RETRY_BASE_DELAY_SECS`       | `1`                             | > 0                                                    | Base delay in seconds for exponential retry backoff.                                                                                                                                                                                                                                                                                                                                                  |
| `JUST_AGENT_POLICY_PRESET`               | _(unset — uses default policy)_ | `allow-all` or `ask-all`                               | Named policy preset for root agents. Takes precedence over `JUST_AGENT_ALLOW_TOOLS`. `allow-all` allows all tools; `ask-all` requires approval for every tool. Only affects root agents at creation time. Subagents inherit their supervisor's policy.                                                                                                                                                |
| `JUST_AGENT_ALLOW_TOOLS`                 | _(unset — uses default policy)_ | Comma-separated tool names                             | Debug override: comma-separated list of tool names to force-allow. Disables `Classify` behavior for `bash_exec`; all unlisted tools default to `Ask`. Not a full policy language. Only affects root agents. Subagents inherit their supervisor's policy.                                                                                                                                              |
| `JUST_AGENT_ROOT_AGENT_PERMISSION_CLASS` | `normal`                        | `normal` or `guest`                                    | Debug override: sandbox permission class for root agents. `normal` = home broad-write + workspace write; `guest` = readonly workspace, no home write. Only affects root agents at creation time; subagents derive their class from their model tier, and restored agents use their persisted `meta.json`. The env form is lowercase; `meta.json` stores the PascalCase serde form (`Normal`/`Guest`). |
| `JUST_AGENT_TOKEN_BUDGET_WARNINGS`       | `80,95`                         | Comma-separated `1`–`99`, sorted ascending, ≥ 1 value  | Token budget usage thresholds (percentage) at which the agent receives a warning message.                                                                                                                                                                                                                                                                                                             |

Source: [`crates/just-agent-runtime/src/config.rs`](../../crates/just-agent-runtime/src/config.rs).

### Inter-variable constraints

Some variables have cross-validation rules enforced at startup for the implicit-profile window (a config-file profile's window is checked per-profile at spawn, not at daemon startup):

- `OUTPUT_RESERVE_TOKENS` must be strictly less than the active context window.
- `SUMMARY_MAX_TOKENS` must not exceed the pinned budget, calculated as `(context_window − OUTPUT_RESERVE_TOKENS) × PINNED_BUDGET_RATIO`.

These are checked at startup against the implicit-profile window (`CONTEXT_WINDOW_TOKENS`); a config-file profile's window is checked per-profile at spawn (and again, lazily, on within-tier failover) — config-file profile windows were never validated at daemon startup.

- `CONTEXT_THRESHOLDS` must have at least 2 values, sorted ascending, each in `1`–`99`.
- `TOKEN_BUDGET_WARNINGS` must have at least 1 value, sorted ascending, each in `1`–`99`.

## Daemon

These variables control the daemon server.

| Variable                        | Required | Default                       | Description                                                                                                                                                                                                                                         |
| ------------------------------- | -------- | ----------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_DAEMON_ADDR`        | no       | `127.0.0.1:3000`              | Listen address for the daemon HTTP server. Set to `0.0.0.0:3000` for container deployments.                                                                                                                                                         |
| `JUST_AGENT_ADVERTISE_URL`      | no       | `http://127.0.0.1:3000`       | URL that agents use to reach this daemon. Injected into shell sessions as `JUST_AGENT_DAEMON_URL`.                                                                                                                                                  |
| `JUST_AGENT_PROMPT_QUEUE_SIZE`  | no       | `5`                           | Max queued messages per agent (message channel capacity). When full, `send_message` returns 503.                                                                                                                                                    |
| `JUST_AGENT_MAX_AGENTS`         | no       | `50`                          | Max concurrent agent instances. Range: 1..=1000. Creation returns 503 when at capacity. Restore is always exempt.                                                                                                                                   |
| `JUST_AGENT_MAX_SUBAGENTS`      | no       | `20`                          | Max direct subagents per agent. Range: 1..=100. Creation returns 503 when the supervisor is at capacity.                                                                                                                                            |
| `JUST_AGENT_MAX_BODY_SIZE_KB`   | no       | `1024`                        | Max HTTP request body size in kilobytes. `0` = axum default (2 MB). Oversized requests return 413.                                                                                                                                                  |
| `JUST_AGENT_OPERATOR_TOKEN`     | no       | _(random `sk-operator-…`)_    | Pre-set the daemon operator token. When unset, a random 256-bit `sk-operator-…` token is generated and printed to stdout. The daemon retains only its SHA-256. Set this for automation where the token must be known in advance; must not be empty. |
| `JUST_AGENT_LLM_API_USER_AGENT` | no       | `just-agent/<daemon-version>` | User-Agent header sent on outbound LLM chat completion requests. Override verbatim (leading/trailing whitespace preserved); illegal header chars (e.g. newlines) fail fast (at startup for the active set, lazily on first failover).               |

Source: [`crates/just-agent-daemon/src/args.rs`](../../crates/just-agent-daemon/src/args.rs).

### Variables injected into agent shell sessions

The daemon injects these into each agent's shell environment so that CLI commands run inside an agent's shell can communicate with the daemon. They are not set by the operator — the daemon provides them automatically.

| Variable                | Injection point                     | Description                                                                                                                                                                                                                                                                                              |
| ----------------------- | ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_DAEMON_URL` | Daemon process (`main.rs`)          | Copied from `JUST_AGENT_ADVERTISE_URL` at startup via `set_var`. Inherited by child processes. Read by CLI and TUI clients to connect.                                                                                                                                                                   |
| `JUST_AGENT_AUTH_TOKEN` | Per-agent shell (`routes/agent.rs`) | Generated 256-bit `sk-agent-…` authentication token. Injected into shell sessions so the agent can call back to the daemon; the daemon stores and compares only its SHA-256. The CLI requires it; the TUI prompts interactively if unset.                                                                |
| `JUST_AGENT_ID`         | Per-agent shell (`routes/agent.rs`) | UUID of the current agent. Available inside agent shells. Read by the CLI for the `skill` (incl. `skill promote`) and `aide` subcommands (where it identifies the acting supervisor), and as the self-target for `activity`. Still read by `just-agent-run` to set `created_by` for scripted child runs. |

### `ADVERTISE_URL` vs `DAEMON_URL`

These serve related but distinct purposes:

- **`JUST_AGENT_ADVERTISE_URL`** — configured by the operator. Tells the daemon "this is the URL others should use to reach you." The daemon injects this value into child processes.
- **`JUST_AGENT_DAEMON_URL`** — consumed by clients (CLI, TUI). Tells them "where is the daemon." Automatically set from `ADVERTISE_URL` by the daemon at startup.

In the common case (everything on localhost) they have the same value. They diverge in container or reverse-proxy setups where the internal listen address differs from the externally reachable URL.

## Data and Skills

| Variable                 | Required | Default                              | Description                                                                                                                                                                     |
| ------------------------ | -------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_DATA_DIR`    | no       | Platform data dir (`~/.local/share`) | Custom base directory for agents, logs, and skills. The runtime appends `just-agent/agents/`, `just-agent/archived/` (removed agents), and `just-agent/skills/` subdirectories. |
| `JUST_AGENT_SKILLS_ROOT` | no       | `DATA_DIR/just-agent/skills/`        | Direct path to the shared skill directory. Used as-is (no suffix appended). Checked before `JUST_AGENT_DATA_DIR` and the platform default.                                      |

Source: [`crates/just-agent-runtime/src/persistence.rs`](../../crates/just-agent-runtime/src/persistence.rs), [`crates/just-agent-runtime/src/tools/skill/mod.rs`](../../crates/just-agent-runtime/src/tools/skill/mod.rs).

## Logging

| Variable   | Required | Default | Description                                                                                                              |
| ---------- | -------- | ------- | ------------------------------------------------------------------------------------------------------------------------ |
| `RUST_LOG` | no       | `info`  | Standard `tracing_subscriber::EnvFilter`. Controls log verbosity for daemon and TUI. Example: `just_agent_client=debug`. |

## System environment variables

The stateless shell backend reads these from the process environment and passes them into clean shell sessions:

| Variable | Fallback     | Purpose                                        |
| -------- | ------------ | ---------------------------------------------- |
| `SHELL`  | `/bin/bash`  | User's login shell for shell session creation. |
| `HOME`   | _(required)_ | User home directory.                           |
| `PATH`   | _(required)_ | System PATH.                                   |

The stateless backend also hardcodes `TERM=dumb`, `NO_COLOR=1`, `LS_COLORS=""`, `CLICOLOR="0"` into every session to suppress color output.
