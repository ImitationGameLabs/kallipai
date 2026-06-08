# Environment variable reference

All configuration is done through environment variables. Copy `.env.example` to `.env` and fill in the required values. If you use `direnv`, it loads `.env` automatically via `.envrc`.

## LLM Provider

These variables select and configure the LLM backend. They are **required** for any agent runtime.

| Variable                          | Required    | Default          | Description                                                                                               |
| --------------------------------- | ----------- | ---------------- | --------------------------------------------------------------------------------------------------------- |
| `JUST_LLM_PROVIDER`               | **yes**     | —                | LLM backend. Supported values: `deepseek`, `openai-compatible`.                                           |
| `JUST_LLM_MODEL`                  | **yes**     | —                | Model identifier passed to the provider (e.g. `deepseek-v4-flash`, `glm-5.1`).                            |
| `JUST_LLM_DEEPSEEK_API_KEY`       | conditional | —                | API key for the DeepSeek provider. Required when `JUST_LLM_PROVIDER=deepseek`.                            |
| `JUST_LLM_DEEPSEEK_BASE_URL`      | no          | DeepSeek default | Override the default DeepSeek API endpoint.                                                               |
| `JUST_LLM_OPENAI_COMPAT_API_KEY`  | conditional | —                | API key for the OpenAI-compatible provider. Required when `JUST_LLM_PROVIDER=openai-compatible`.          |
| `JUST_LLM_OPENAI_COMPAT_BASE_URL` | conditional | `""`             | Override the default OpenAI-compatible API endpoint. Required when `JUST_LLM_PROVIDER=openai-compatible`. |

Source: [`crates/just-agent-runtime/src/provider.rs`](../../crates/just-agent-runtime/src/provider.rs).

## Agent Core

Runtime tuning parameters. All are optional with sensible defaults.

| Variable                           | Default                         | Constraints                                            | Description                                                                                                                                                                                                                                                       |
| ---------------------------------- | ------------------------------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_SYSTEM_PROMPT`         | Built-in prompt                 | —                                                      | System prompt injected into every LLM session. See `DEFAULT_SYSTEM_PROMPT` in `config.rs` for the built-in text.                                                                                                                                                  |
| `JUST_AGENT_MAX_TOOL_ROUNDS`       | `32`                            | > 0                                                    | Maximum tool-call rounds per agent.                                                                                                                                                                                                                               |
| `JUST_AGENT_WORKSPACE_ROOT`        | Current directory               | —                                                      | Root directory for agent workspace. Can also be set via CLI `--workspace-root`. CLI flag takes precedence.                                                                                                                                                        |
| `JUST_AGENT_CONTEXT_WINDOW_TOKENS` | `128000`                        | > 0                                                    | Context window size in tokens.                                                                                                                                                                                                                                    |
| `JUST_AGENT_OUTPUT_RESERVE_TOKENS` | `8192`                          | < `CONTEXT_WINDOW_TOKENS`                              | Tokens reserved for model output within the context window.                                                                                                                                                                                                       |
| `JUST_AGENT_SUMMARY_MAX_TOKENS`    | `1200`                          | > 0, ≤ pinned budget                                   | Maximum tokens for compacted (summarized) context. Must fit within the pinned budget (effective budget × pinned budget ratio).                                                                                                                                    |
| `JUST_AGENT_TOOL_TIMEOUT_SECS`     | `120`                           | —                                                      | Timeout in seconds for individual tool executions.                                                                                                                                                                                                                |
| `JUST_AGENT_PINNED_BUDGET_RATIO`   | `0.25`                          | 0.0–1.0 (exclusive)                                    | Fraction of effective budget allocated to pinned context items.                                                                                                                                                                                                   |
| `JUST_AGENT_CONTEXT_THRESHOLDS`    | `50,60,70,80`                   | Comma-separated `1`–`99`, sorted ascending, ≥ 2 values | Context usage thresholds (percentage). The last value triggers auto-compact; preceding values are warnings.                                                                                                                                                       |
| `JUST_AGENT_MAX_RETRIES`           | `3`                             | —                                                      | Maximum retries for LLM API calls.                                                                                                                                                                                                                                |
| `JUST_AGENT_RETRY_BASE_DELAY_SECS` | `1`                             | > 0                                                    | Base delay in seconds for exponential retry backoff.                                                                                                                                                                                                              |
| `JUST_AGENT_ALLOW_TOOLS`           | _(unset — uses default policy)_ | Comma-separated tool names                             | Debug override: comma-separated list of tool names to force-allow. Disables `Classify` behavior for `shell_session_exec`; all unlisted tools default to `Ask`. Not a full policy language. Only affects root agents. Subagents inherit their supervisor's policy. |
| `JUST_AGENT_TOKEN_BUDGET_WARNINGS` | `80,95`                         | Comma-separated `1`–`99`, sorted ascending, ≥ 1 value  | Token budget usage thresholds (percentage) at which the agent receives a warning message.                                                                                                                                                                         |

Source: [`crates/just-agent-runtime/src/config.rs`](../../crates/just-agent-runtime/src/config.rs).

### Inter-variable constraints

Some variables have cross-validation rules enforced at startup:

- `OUTPUT_RESERVE_TOKENS` must be strictly less than `CONTEXT_WINDOW_TOKENS`.
- `SUMMARY_MAX_TOKENS` must not exceed the pinned budget, calculated as `(CONTEXT_WINDOW_TOKENS − OUTPUT_RESERVE_TOKENS) × PINNED_BUDGET_RATIO`.
- `CONTEXT_THRESHOLDS` must have at least 2 values, sorted ascending, each in `1`–`99`.
- `TOKEN_BUDGET_WARNINGS` must have at least 1 value, sorted ascending, each in `1`–`99`.

## Daemon

These variables control the daemon server.

| Variable                       | Required | Default                 | Description                                                                                                       |
| ------------------------------ | -------- | ----------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_DAEMON_ADDR`       | no       | `127.0.0.1:3000`        | Listen address for the daemon HTTP server. Set to `0.0.0.0:3000` for container deployments.                       |
| `JUST_AGENT_ADVERTISE_URL`     | no       | `http://127.0.0.1:3000` | URL that agents use to reach this daemon. Injected into PTY sessions as `JUST_AGENT_DAEMON_URL`.                  |
| `JUST_AGENT_PROMPT_QUEUE_SIZE` | no       | `5`                     | Max queued messages per agent (message channel capacity). When full, `send_message` returns 503.                  |
| `JUST_AGENT_MAX_AGENTS`        | no       | `50`                    | Max concurrent agent instances. Range: 1..=1000. Creation returns 503 when at capacity. Restore is always exempt. |
| `JUST_AGENT_MAX_SUBAGENTS`     | no       | `20`                    | Max direct subagents per agent. Range: 1..=100. Creation returns 503 when the supervisor is at capacity.          |
| `JUST_AGENT_MAX_BODY_SIZE_KB`  | no       | `1024`                  | Max HTTP request body size in kilobytes. `0` = axum default (2 MB). Oversized requests return 413.                |

Source: [`crates/just-agent-daemon/src/args.rs`](../../crates/just-agent-daemon/src/args.rs).

### Variables injected into agent PTY sessions

The daemon injects these into each agent's PTY environment so that CLI commands run inside an agent's shell can communicate with the daemon. They are not set by the operator — the daemon provides them automatically.

| Variable                | Injection point                   | Description                                                                                                                                                                                        |
| ----------------------- | --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `JUST_AGENT_DAEMON_URL` | Daemon process (`main.rs`)        | Copied from `JUST_AGENT_ADVERTISE_URL` at startup via `set_var`. Inherited by child processes. Read by CLI and TUI clients to connect.                                                             |
| `JUST_AGENT_AUTH_TOKEN` | Per-agent PTY (`routes/agent.rs`) | Generated UUID authentication token. Injected into PTY sessions; also printed to daemon stdout for the operator to export to CLI/TUI. The CLI requires it; the TUI prompts interactively if unset. |
| `JUST_AGENT_ID`         | Per-agent PTY (`routes/agent.rs`) | UUID of the current agent. Available inside agent shells. Read by the CLI for `skill` and `promote-request` subcommands, and passed as `created_by` when spawning subagents.                       |

### `ADVERTISE_URL` vs `DAEMON_URL`

These serve related but distinct purposes:

- **`JUST_AGENT_ADVERTISE_URL`** — configured by the operator. Tells the daemon "this is the URL others should use to reach you." The daemon injects this value into child processes.
- **`JUST_AGENT_DAEMON_URL`** — consumed by clients (CLI, TUI). Tells them "where is the daemon." Automatically set from `ADVERTISE_URL` by the daemon at startup.

In the common case (everything on localhost) they have the same value. They diverge in container or reverse-proxy setups where the internal listen address differs from the externally reachable URL.

## Data and Skills

| Variable                 | Required | Default                              | Description                                                                                                                                |
| ------------------------ | -------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `JUST_AGENT_DATA_DIR`    | no       | Platform data dir (`~/.local/share`) | Custom base directory for agents, logs, and skills. The runtime appends `just-agent/agents/` and `just-agent/skills/` subdirectories.      |
| `JUST_AGENT_SKILLS_ROOT` | no       | `DATA_DIR/just-agent/skills/`        | Direct path to the shared skill directory. Used as-is (no suffix appended). Checked before `JUST_AGENT_DATA_DIR` and the platform default. |

Source: [`crates/just-agent-runtime/src/persistence.rs`](../../crates/just-agent-runtime/src/persistence.rs), [`crates/just-agent-runtime/src/tools/skill/mod.rs`](../../crates/just-agent-runtime/src/tools/skill/mod.rs).

## Logging

| Variable   | Required | Default | Description                                                                                                              |
| ---------- | -------- | ------- | ------------------------------------------------------------------------------------------------------------------------ |
| `RUST_LOG` | no       | `info`  | Standard `tracing_subscriber::EnvFilter`. Controls log verbosity for daemon and TUI. Example: `just_agent_client=debug`. |

## System environment variables

The PTY shell backend reads these from the process environment and passes them into clean shell sessions:

| Variable | Fallback     | Purpose                                      |
| -------- | ------------ | -------------------------------------------- |
| `SHELL`  | `/bin/bash`  | User's login shell for PTY session creation. |
| `HOME`   | _(required)_ | User home directory.                         |
| `PATH`   | _(required)_ | System PATH.                                 |

The PTY backend also hardcodes `TERM=dumb`, `NO_COLOR=1`, `LS_COLORS=""`, `CLICOLOR="0"` into every session to suppress color output.
