# Environment variable reference

All configuration is done through environment variables. Copy `.env.example` to
`.env` and fill in the required values. If you use `direnv`, it loads `.env`
automatically via `.envrc`.

## LLM Provider

These variables select and configure the LLM backend. They are **required** when
no [model profiles](#model-profiles) config file is present.

The env path supports `deepseek` and `openai-compatible` only; the `openai-responses` and `anthropic` families are reachable through a [model profiles](#model-profiles) config file.

| Variable                            | Required    | Default          | Description                                                                                                 |
| ----------------------------------- | ----------- | ---------------- | ----------------------------------------------------------------------------------------------------------- |
| `KALLIP_LLM_PROVIDER`               | **yes**     | —                | LLM backend. Supported values: `deepseek`, `openai-compatible`.                                             |
| `KALLIP_LLM_MODEL`                  | **yes**     | —                | Model identifier passed to the provider (e.g. `deepseek-v4-flash`, `glm-5.1`).                              |
| `KALLIP_LLM_DEEPSEEK_API_KEY`       | conditional | —                | API key for the DeepSeek provider. Required when `KALLIP_LLM_PROVIDER=deepseek`.                            |
| `KALLIP_LLM_DEEPSEEK_BASE_URL`      | no          | DeepSeek default | Override the default DeepSeek API endpoint.                                                                 |
| `KALLIP_LLM_OPENAI_COMPAT_API_KEY`  | conditional | —                | API key for the OpenAI-compatible provider. Required when `KALLIP_LLM_PROVIDER=openai-compatible`.          |
| `KALLIP_LLM_OPENAI_COMPAT_BASE_URL` | conditional | `""`             | Override the default OpenAI-compatible API endpoint. Required when `KALLIP_LLM_PROVIDER=openai-compatible`. |

## Model Profiles

A profile binds a model to an endpoint and its declared capabilities
(`max_context_window`), grouped into named sets. With a profiles config
file, the tagma loads multiple provider/model combinations, each profile
declaring its own `max_context_window`.
Each profile may also declare `modalities`; an omitted declaration
defaults to text-only, and a set's effective modalities are the
intersection across its members.

The profiles config file lives at the instance config root's
    `profiles/profiles.toml` — `<config home>/kallipai/tagmata/<slug>/`.
    A config root that cannot be derived (no `KALLIP_TAGMA_SLUG`, no config
    home) degrades the read to the implicit env profile with a warning;
    a write with no config root errors out. Without a config file (the
    default for benchmark/scripting via Harbor and `kallip-run`), a single
its `max_context_window` is derived from `KALLIP_CONTEXT_WINDOW_TOKENS` (default
`128000`).

Example `profiles.toml`:

```toml
default = "primary"

[endpoints.deepseek-primary]
family = "deepseek"
api_key = "${KALLIP_LLM_DEEPSEEK_API_KEY}" # env-var indirection keeps secrets out of the file

[endpoints.openrouter]
family = "openai-compatible"
api_key = "${OPENROUTER_API_KEY}"
base_url = "https://openrouter.ai/api/v1"

[endpoints.official-openai]
family = "openai-responses"
api_key = "${OPENAI_API_KEY}" # no base_url: talks to the official endpoint

[sets.primary]
description = "full-capability work"
  [[sets.primary.profiles]]
  id = "deepseek-v4-pro"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-pro"
  max_context_window = 500000

[sets.fast]
description = "cheap delegation"
  [[sets.fast.profiles]]
  id = "deepseek-v4-flash"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-flash"
  max_context_window = 128000
```

- `family` must be one of `deepseek`, `openai-compatible`, `openai-responses`, `anthropic`.
- `base_url` is required for `openai-compatible` and optional elsewhere: every other family falls back to its official endpoint when omitted.
- `store` (optional, default `true`) keeps server-side conversation storage on, so `openai-responses` continues each turn from the stored chain; `store = false` (or any non-Responses family) replays the full conversation every turn.
- `effort` (optional) requests a reasoning effort — `low`, `medium`, `high`, `xhigh`, or `max`. Providers honor it within their own limits (DeepSeek officially downgrades `medium` and `xhigh` to `high` per its [API reference](https://api-docs.deepseek.com/api/create-chat-completion)).
- `${VAR}` in `api_key` / `base_url` is expanded from the process environment.
- The config file should be `chmod 600` (the tagma warns if
  group/other-readable, since it may hold API keys).

### Set selection

Sets are addressed by name: the root agent is bound to the config's `default`
set, and each subagent spawn declares its set explicitly via `profile_set`
(an unknown name is rejected). The binding is recorded on the agent record,
so set order in the file carries no meaning. Renaming or deleting a set
leaves bound agents dangling — restore tolerates the placeholder, but
prompt delivery rejects with `409` until the set returns under that name.

The `default` marker resolves at load: an explicit `default = "<name>"` must
name an existing set (a name matching nothing is a config error); with exactly
one set and no marker, that set becomes the default and the tagma writes the
marker back into the file — inserted ahead of the first table header, so
comments, key order, and `${VAR}` spellings survive; several sets with no
marker is a config error (name one explicitly). A hand-written empty
marker (`default = ""`) reads back as unmarked and takes the single-set
auto-mark path rather than erroring. In that corner the file keeps its
explicit empty marker — the auto-mark write-back is skipped, so the
resolved default is memory-only. An empty `sets` table boots
profile-less.

The selected set's first profile is the active model; the remaining profiles
form a within-set failover chain. When the active profile fails terminally
(HTTP 401/403/404, or transient retries exhausted), the runner advances to the
next profile in the set and retries the same turn; a request-level failure
(400/422) errors the round instead. The active profile index sticks for the
agent's lifetime and resets to 0 on restore.

On advance, the context window tracks the new profile's declared
`max_context_window` (within-set windows may differ — placing models with
different windows in one set is supported). If the carried context now exceeds
the new (possibly smaller) window, the runner compacts it before retrying, so
the turn survives the switch. A candidate whose window would violate a budget
invariant is skipped _before_ the advance (so the agent never sends an oversized
request to a smaller-window model); if no feasible candidate remains, the chain
is reported `allCandidatesInfeasible` (tune `SUMMARY_MAX_TOKENS` /
`PINNED_BUDGET_RATIO` or raise the window). The _active_ profile's window (not a
failover candidate) is validated at spawn — a window that violates a budget
invariant rejects the spawn outright (fail-fast) rather than silently falling
back.

The retry budget is **per-endpoint**, not per-profile: rate limits are
endpoint-scoped, so two profiles sharing one endpoint share one budget. A
profile's transient retries accumulate within `retry_timeout` **across rounds**
— this is intentional rate-limit backpressure (a persistently failing endpoint
gets fewer retries, forcing failover or a round error), and it matches the
pre-failover agent-wide behavior for the active profile. The index only advances
forward, so a failed-over-from endpoint's accumulated budget never re-bites.

Edge case: an agent whose recorded set name matches no live set dangles — the
record keeps the name, restore tolerates it as a placeholder, but prompt
delivery rejects with `409` until the set returns under that name.

Source:
[`crates/kallip-runtime/src/profile/`](../../crates/kallip-runtime/src/profile).

## Agent Core

Runtime tuning parameters. All are optional with sensible defaults. (The
identity vars `KALLIP_ID` / `KALLIP_SUPERVISOR_AGENT_ID` /
`KALLIP_ROOT_AGENT_ID` are tagma-injected, not tuned here — see
[Variables injected into agent shell sessions](#variables-injected-into-agent-shell-sessions).)

> **Tagma root agent:** the tagma owns a single root agent, eagerly created at
> startup. The root's config is read from the env at that moment — notably
> `KALLIP_WORKSPACE_ROOT`, `KALLIP_MAX_TOOL_ROUNDS`, and
> `KALLIP_ROOT_AGENT_PERMISSION_CLASS` (below). Clients fetch it via
> `GET /agents/root`; they never create it.

| Variable                                       | Default                               | Constraints                                            | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ---------------------------------------------- | ------------------------------------- | ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_SYSTEM_PROMPT`                         | Built-in prompt                       | —                                                      | Overrides the **static base** of the system prompt (the posture + tool/round-model section; see `DEFAULT_SYSTEM_PROMPT` in `config.rs`). The per-agent `# Your identity` section is always tagma-injected at the head of this base and is not env-configurable. Must remain constant across the tagma's lifetime — root and all subagents resolve this env var identically, and the byte-identical static tail is what keeps prompt-prefix caching effective across agents.                                                                                                                                                                                                                                                                                                                                              |
| `KALLIP_MAX_TOOL_ROUNDS`                       | _(unlimited)_                         | > 0                                                    | Maximum tool-call rounds per agent. Defaults to unlimited — the tagma-wide token budget is the primary safety net. Set this to enforce a hard round limit independent of token consumption (e.g. for testing or cost control). Note: this does NOT bound heartbeat rounds (a bare-assistant response re-loops without counting here); see `KALLIP_MAX_HEARTBEAT_ROUNDS`.                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `KALLIP_MAX_HEARTBEAT_ROUNDS`                  | `3`                                   | > 0                                                    | Max consecutive heartbeat rounds (bare-assistant re-loops) before the harness force-idles the agent. The agent only parks by calling `break`; a bare response with no tool call does not end the run — the harness injects a heartbeat prompt and continues. This guardrail bounds "self-monologue" token burn.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `KALLIP_MAX_TRANSIENT_RETRIES`                 | `3`                                   | > 0                                                    | Max consecutive transient (failover-chain-exhausted) parks that earn a timed retry before the agent hard-parks and surfaces to the operator. Bounded additionally by the retry policy's `retry_timeout` wall clock (300s default, `KALLIP_RETRY_TIMEOUT_SECS`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `KALLIP_TAGMA_RELAY_ARCHEION_URL`                 | _(unset)_                             | HTTPS URL                                              | The public URL of the archeion the tagma's in-process relay connector reaches for enrollment (first run only; the stored tagma token is reused thereafter). Setting this activates the relay. If enrollment fails (missing code, unreachable archeion), the tagma degrades to local-only: it logs an error, keeps serving local agents, and the lesche message route returns 503. Unset = pure-local, no relay.                                                                                                                                                                                                                                                                                                                                                                                                                |
| `KALLIP_TAGMA_RELAY_LESCHE_URL`                | `KALLIP_TAGMA_RELAY_ARCHEION_URL` origin | HTTPS URL                                              | The public URL of the lesche data-plane relay the tagma holds its tunnel against and posts envelopes / key-exchange responses to. Defaults to the `KALLIP_TAGMA_RELAY_ARCHEION_URL` origin when unset (same-origin topologies only); set explicitly for the per-service subdomain topology (e.g. `https://lesche.kallipai.com`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `KALLIP_TAGMA_RELAY_ENROLLMENT_CODE`           | _(unset)_                             | `sk-enroll-...`                                        | A single-use enrollment code minted via the archeion dashboard (after a user signs up). Required on the tagma's first boot when `KALLIP_TAGMA_RELAY_ARCHEION_URL` is set; the enrollment origin is recorded under the instance data root's `credentials/` alongside the tagma token, so the code can be removed once the token is stored. A code left set with stored credentials is tolerated at the same archeion (ignored with a loud warning) but fails the boot at a different archeion with both addresses named. The kallip-daemon start path filters a spent code from the replayed env and scrubs it from the daemon's registration record. |
| `KALLIP_TAGMA_RELAY_MESSAGE_BURST_MAX`         | `20`                                  | > 0                                                    | Max `kallip lesche send` deliveries per burst window. Bounds a runaway agent message loop. Process-global today (one root agent = one conversation, so per-process == per-conversation); a future multi-root relay would scope this per agent/turn.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `KALLIP_TAGMA_RELAY_MESSAGE_BURST_WINDOW_SECS` | `10`                                  | > 0                                                    | Length in seconds of the message burst window paired with `KALLIP_TAGMA_RELAY_MESSAGE_BURST_MAX`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `KALLIP_TAGMA_RELAY_HISTORY_TTL_DAYS`          | `30`                                  | > 0                                                    | Chat-history retention in days. Rows older than this are GC'd. This is the _normal_ retention boundary — what a reconnecting device can re-pull. A device offline past the window only sees what remains.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `KALLIP_TAGMA_RELAY_HISTORY_CAP`               | `100000`                              | > 0                                                    | Chat-history row cap. When exceeded, the oldest rows are trimmed regardless of age. **Not a usage quota** — a runaway backstop (a buggy agent spamming, an attack); normal use within the TTL window should never reach it, and a `warn!` fires if it trims.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `KALLIP_WORKSPACE_ROOT`                        | Current directory                     | —                                                      | Root directory for agent workspace. Read by the tagma at startup to configure the singleton root agent; subagents derive their workspace from their supervisor (`POST /agents` `workspace_root`, within the supervisor's workspace).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `KALLIP_CONTEXT_WINDOW_TOKENS`                 | `128000`                              | > 0                                                    | Context window size in tokens for the implicit env profile (no [profiles config](#model-profiles)) — it becomes that profile's `max_context_window`. With a config file, each profile declares its own `max_context_window` instead.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `KALLIP_OUTPUT_RESERVE_TOKENS`                 | `8192`                                | < `CONTEXT_WINDOW_TOKENS`                              | Tokens reserved for model output within the context window.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `KALLIP_SUMMARY_MAX_TOKENS`                    | `1200`                                | > 0, ≤ pinned budget                                   | Maximum tokens for compacted (summarized) context. Must fit within the pinned budget (effective budget × pinned budget ratio).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `KALLIP_TOOL_TIMEOUT_SECS`                     | `120`                                 | —                                                      | Timeout in seconds for individual tool executions.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `KALLIP_PINNED_BUDGET_RATIO`                   | `0.25`                                | 0.0–1.0 (exclusive)                                    | Fraction of effective budget allocated to pinned context items.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `KALLIP_CONTEXT_THRESHOLDS`                    | `50,60,70,80`                         | Comma-separated `1`–`99`, sorted ascending, ≥ 2 values | Context usage thresholds (percentage). The last value triggers auto-compact; preceding values are warnings.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `KALLIP_MAX_RETRIES`                           | `10`                                  | ≤ 1000                                                 | Maximum retries for LLM API calls. Upper bound guards the attempt-budget arithmetic (`u32::MAX` would wrap to zero attempts).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `KALLIP_RETRY_BASE_DELAY_SECS`                 | `1`                                   | > 0, ≤ 3600                                            | Base delay in seconds for exponential retry backoff.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `KALLIP_RETRY_MAX_DELAY_SECS`                  | `60`                                  | > 0, ≤ 3600                                            | Cap in seconds for a single retry backoff.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `KALLIP_RETRY_TIMEOUT_SECS`                    | `300`                                 | > 0, ≤ 86400                                           | Overall deadline in seconds for one retry sequence. Upper bound avoids `Instant + Duration` overflow on the deadline.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `KALLIP_POLICY_PRESET`                         | _(unset — `default`)_                 | `default`, `auto`, or `allow-all`                      | Tagma-global `bash_exec` classify preset, read once at startup and immutable for the tagma's lifetime. Every agent (root and subagent) runs under this preset. `default` (also when unset): catalog commands allow, unclassified commands ask, the builtin command denylist (`sed`, `awk`, `ed`, `ex`) and structural rejects (e.g. `curl \| sh`) deny. `auto` is the practical permissive mode: unclassified commands allow too, while the denylist and structural rejects still deny. `allow-all` is a **debug preset, not for production**: the classifier short-circuits to allow every parseable command, so the denylist and structural rejects do not apply. Per-command overrides are configured separately via `ExecPolicy` (`PUT /agents/{id}/exec-policy`). See _Classify presets_ in `docs/architecture.md`. |
| `KALLIP_ROOT_AGENT_PERMISSION_CLASS`           | `normal`                              | `normal` or `guest`                                    | Debug override: sandbox permission class for root agents. `normal` = home broad-write + workspace write; `guest` = readonly workspace, no home write. Only affects root agents at creation time; subagents require an explicit `permission_class` on `POST /agents` / `kallip subagent spawn --permission-class`, granted at most the supervisor's own class; restored agents use their persisted `meta.json`. The env form is lowercase; `meta.json` stores the PascalCase serde form (`Normal`/`Guest`). |
| `KALLIP_TOKEN_BUDGET_WARNINGS`                 | `80,95`                               | Comma-separated `1`–`99`, sorted ascending, ≥ 1 value  | Token budget usage thresholds (percentage) at which the agent receives a warning message.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |

Source:
[`crates/kallip-runtime/src/config.rs`](../../crates/kallip-runtime/src/config.rs).

> **Client-side cache (not env-configurable).** The app keeps a per-device
> IndexedDB cache (`kallip-relay` DB) of already-rendered chat lines so a
> refresh restores the conversation instantly and only asks the tagma for an
> incremental delta. It is a disposable derived mirror (re-pulled on demand),
> stored **plaintext** under the same device/host-trust model as the tagma's
> SQLite store, and cleared on logout. It has no environment variable.

### Inter-variable constraints

Some variables have cross-validation rules enforced at startup for the
implicit-profile window (a config-file profile's window is checked per-profile
at spawn, not at tagma startup):

- `OUTPUT_RESERVE_TOKENS` must be strictly less than the active context window.
- `SUMMARY_MAX_TOKENS` must not exceed the pinned budget, calculated as
  `(context_window − OUTPUT_RESERVE_TOKENS) × PINNED_BUDGET_RATIO`.

These are checked at startup against the implicit-profile window
(`CONTEXT_WINDOW_TOKENS`); a config-file profile's window is checked per-profile
at spawn (and again, lazily, on within-set failover) — config-file profile
windows were never validated at tagma startup.

- `CONTEXT_THRESHOLDS` must have at least 2 values, sorted ascending, each in
  `1`–`99`.
- `TOKEN_BUDGET_WARNINGS` must have at least 1 value, sorted ascending, each in
  `1`–`99`.

## Tagma

These variables control the tagma server.

| Variable                    | Required | Default                    | Description                                                                                                                                                                                                                                       |
| --------------------------- | -------- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_TAGMA_ADDR`         | no       | `127.0.0.1:3000`           | Listen address for the tagma HTTP server. Set to `0.0.0.0:3000` for container deployments. Managed instances receive `127.0.0.1:0` from the daemon unless the spawn/start request, the harvested environment, or the persisted record replay pins the key (composition-time `SocketAddr` shape check across channels; an occupied pinned port surfaces as a spawn timeout rollback or a start failure pointing at the logs). A pin ending in `:0` rebinds a fresh dynamic port on every boot. |
| `KALLIP_ADVERTISE_URL`      | no       | `http://127.0.0.1:3000`    | URL that agents use to reach this tagma. Injected into shell sessions as `KALLIP_TAGMA_URL`.                                                                                                                                                      |
| `KALLIP_PROMPT_QUEUE_SIZE`  | no       | `5`                        | Max queued messages per agent (message channel capacity). When full, `send_message` returns 503.                                                                                                                                                  |
| `KALLIP_MAX_AGENTS`         | no       | `50`                       | Max concurrent agent instances. Range: 1..=1000. Creation returns 503 when at capacity. Restore is always exempt.                                                                                                                                 |
| `KALLIP_MAX_SUBAGENTS`      | no       | `20`                       | Max direct subagents per agent. Range: 1..=100. Creation returns 503 when the supervisor is at capacity.                                                                                                                                          |
| `KALLIP_MAX_BODY_SIZE_KB`   | no       | `1024`                     | Max HTTP request body size in kilobytes. `0` = axum default (2 MB). Oversized requests return 413.                                                                                                                                                |
| `KALLIP_TOKEN_BUDGET`      | no       | _(unlimited)_              | Startup value for the tagma-wide token spend cap (see the Token Budget section in `docs/reference/tagma-api.md`). Accepts plain token counts or `K`/`M`/`G` suffixes (e.g. `500000`, `5M`, `2G`). `0` boots the tagma paused — all agents block until a finite budget is set via `POST /budget`. Unset boots with no cap; an empty or invalid value fails the boot with a parse error rather than silently starting uncapped. The runtime budget is read and changed via `GET`/`POST /budget`, not this variable. |
| `KALLIP_OPERATOR_TOKEN`     | no       | _(random `sk-operator-…`)_ | Pre-set the tagma operator token. When unset, a random 256-bit `sk-operator-…` token is generated and printed to stdout. The tagma retains only its SHA-256. Set this for automation where the token must be known in advance; must not be empty. |
| `KALLIP_LLM_API_USER_AGENT` | no       | `kallip/<tagma-version>`   | User-Agent header sent on outbound LLM chat completion requests. Override verbatim (leading/trailing whitespace preserved); illegal header chars (e.g. newlines) fail fast (at startup for the active set, lazily on first failover).             |

Source:
[`crates/kallip-tagma/src/args.rs`](../../crates/kallip-tagma/src/args.rs).

### Variables injected into agent shell sessions

The tagma injects these into each agent's shell environment so that CLI commands
run inside an agent's shell can communicate with the tagma. They are not set by
the operator — the tagma provides them automatically.

| Variable                     | Injection point                     | Description                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_TAGMA_URL`           | Tagma process (`main.rs`)           | Copied from `KALLIP_ADVERTISE_URL` at startup via `set_var`. Inherited by child processes. Read by CLI clients to connect.                                                                                                                                                                                                                           |
| `KALLIP_AUTH_TOKEN`          | Per-agent shell (`routes/agent.rs`) | Generated 256-bit `sk-agent-…` authentication token. Injected into shell sessions so the agent can call back to the tagma; the tagma stores and compares only its SHA-256. The CLI requires it.                                                                                                                                                       |
| `KALLIP_ID`                  | Per-agent shell (`routes/agent.rs`) | UUID of the current agent. Available inside agent shells. Read by the CLI for the `skill` and `subagent` subcommands (where it identifies the acting supervisor), and as the self-target for `activity` and `lesche send`.                                                                                                                           |
| `KALLIP_SUPERVISOR_AGENT_ID` | Per-agent shell (`routes/agent.rs`) | The agent's supervisor id (the direct `created_by` delegator). Injected for subagents only — **unset for the root agent** (absent, not empty), so root-ness is detectable by env absence. Surfaces the id so the agent can address its supervisor (e.g. `kallip message <id>`); the CLI takes the id as a positional arg and does not read this var. |
| `KALLIP_ROOT_AGENT_ID`       | Per-agent shell (`routes/agent.rs`) | The tagma root agent id (the agent itself for the root). Injected into every agent's shell. Surfaces the id so the agent can escalate to the root (e.g. `kallip message <id>`); the CLI takes the id as a positional arg and does not read this var.                                                                                                 |

### `ADVERTISE_URL` vs `TAGMA_URL`

These serve related but distinct purposes:

- **`KALLIP_ADVERTISE_URL`** — configured by the operator. Tells the tagma "this
  is the URL others should use to reach you." The tagma injects this value into
  child processes.
- **`KALLIP_TAGMA_URL`** — consumed by clients (CLI). Tells them "where is
  the tagma." Automatically set from `ADVERTISE_URL` by the tagma at startup.

In the common case (everything on localhost) they have the same value. They
diverge in container or reverse-proxy setups where the internal listen address
differs from the externally reachable URL.

## Data and Skills

| Variable             | Required | Default                              | Description                                                                                                                                                                                                                                                                                                                                                                        |
| -------------------- | -------- | ------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| (data root)           | —        | —                                    | Derived from `KALLIP_TAGMA_SLUG` under the platform data dir (see the Local daemon section); there is no per-instance data-dir variable to set. The runtime writes `agents/` (with the `active/`, `inactive/`, and `archived/` life-stage directories) and `skills/` directly under it.                                                                                                                                                                                                                                    |
| `KALLIP_SKILLS_ROOT` | no       | Instance data root's `skills/`       | Direct path to the shared skill directory. Used as-is (no suffix appended).                                                                                                                                                                                                                                                                                                             |
| `KALLIP_SKILLS_SEED` | no       | _(unset; nix wrapper)_              | Read-only tree of bundled skill defaults (a nix store path). On the tagma's first boot, when the shared skill directory is empty, its contents are copied into it. The target is `KALLIP_SKILLS_ROOT` if set, else the instance data root's `skills/` — `KALLIP_SKILLS_ROOT` only relocates the target, it does not disable seeding. Skipped when the target is already non-empty (never clobber). Under a nix install the workspace build ships this default already via its `kallip-tagma` wrapper (`--set-default`: it applies only when the variable is unset, so an explicit env value or the container image Env still wins). An explicitly empty value disables seeding: the wrapper keeps it as-is and the tagma filters it out. |

Source:
[`crates/kallip-runtime/src/persistence.rs`](../../crates/kallip-runtime/src/persistence.rs),
[`crates/kallip-runtime/src/tools/skill/mod.rs`](../../crates/kallip-runtime/src/tools/skill/mod.rs).

## Logging

| Variable   | Required | Default | Description
| ---------- | -------- | ------- | -------------------------------------------------------------------------------------------------------------------
| `RUST_LOG` | no       | `info`  | Standard `tracing_subscriber::EnvFilter`. Controls log verbosity for tagma. Example: `kallip_client=debug`.
| `KALLIP_ARCHEION_LOG_DIR` | no | _(unset — stdout only)_ | Opt-in rolling file log for the archeion. Set to a directory: events are double-written to daily-rotating files (seven kept, `archeion.log.` prefix) while stdout keeps flowing for journald capture. Unset or empty keeps the historical stdout-only behavior; an uncreatable directory degrades to stdout-only with a startup notice. Directory ownership and permissions are a deployment concern (systemd `LogsDirectory`).
| `KALLIP_LESCHE_LOG_DIR` | no | _(unset — stdout only)_ | Opt-in rolling file log for the lesche, identical semantics to `KALLIP_ARCHEION_LOG_DIR` (double-write, daily rotation, seven files kept, `lesche.log.` prefix).
| `KALLIP_FILES_LOG_DIR` | no | _(unset — stdout only)_ | Opt-in rolling file log for the files service, identical semantics to `KALLIP_ARCHEION_LOG_DIR` (double-write, daily rotation, seven files kept, `files.log.` prefix).

## Cron

The timer/notification daemon (`kallip-cron-daemon`) and its management CLI
(`kallip-cron`). The daemon fires schedules and injects them into agent
conversations via the tagma HTTP API. The management API is self-scoped: the
`kallip-cron` CLI runs inside an agent shell and reuses the shell's
`KALLIP_ID` + `KALLIP_AUTH_TOKEN` (both auto-injected by the tagma); the daemon
verifies the pair against the tagma and scopes every operation to that agent's
own schedules.

| Variable                 | Required  | Default                          | Description                                                                                                                                                              |
| ------------------------ | --------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_CRON_ADDR`       | no        | `127.0.0.1:3010`                 | Address the daemon's management API listens on. **Loopback only** — cron is an internal tagma-side service; the daemon refuses a non-loopback bind.                      |
| `KALLIP_CRON_DATA_DIR`   | no        | Platform data dir + `kallipai/cron/` | Directory holding `cron.sqlite`. Unset with no determinable platform data dir, the daemon fails fast instead of guessing.                                                              |
| `KALLIP_CRON_TICK_MS`    | no        | `1000`                           | Scheduler tick interval (ms). Must be `>= 1000` (second-precision scheduler).                                                                                            |
| `KALLIP_CRON_DELIVER_MS` | no        | `500`                            | Deliverer poll interval (ms): how often triggered schedules are pushed to tagma.                                                                                         |
| `KALLIP_CRON_URL`        | no        | `http://127.0.0.1:3010`          | Daemon URL used by the `kallip-cron` CLI client.                                                                                                                         |
| `KALLIP_ID`              | yes (CLI) | _(unset)_                        | The calling agent's id (auto-injected into agent shells by the tagma); the CLI passes it as the self-scope, and the daemon verifies it against the bearer via the tagma. |
| `KALLIP_TAGMA_URL`       | yes       | `http://127.0.0.1:3000`          | Tagma URL for delivery + per-request verify (read by `TagmaClient::from_env`). Reused from the tagma client; not `KALLIP_CRON_*`-prefixed.                               |
| `KALLIP_AUTH_TOKEN`      | yes       | _(unset)_                        | The daemon's operator secret for delivery (fired reminders render `[From: operator]`); the CLI's agent bearer for management requests. Reused from the tagma client.     |

Source:
[`crates/time/kallip-cron-daemon/src/args.rs`](../../crates/time/kallip-cron-daemon/src/args.rs),
[`crates/time/kallip-cron-client/src/client.rs`](../../crates/time/kallip-cron-client/src/client.rs).

## Files service

The file transfer service (`kallip-files`) and its `kallip file` CLI
client. The service owns content-addressed blob storage and record
metadata in its own Postgres; identity and enrollment facts stay in the
archeion, verified per request through the archeion's `/internal/*` surface
with a shared secret. The CLI reads its credentials from the agent
shell's spawn env (no flags carry secrets).

Secrets this platform generates are filed by lifetime: `/var/lib` for
service-owned persistent state (the archeion's internal token, generated
once and only read after), `/run` for volatile runtime credentials (the
auto-generated admin token, reset on every restart), and `/etc` for
administrator-pinned assets.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `KALLIP_FILES_ADDR` | no | `127.0.0.1:7400` | Address the service listens on (behind a TLS-terminating reverse proxy). |
| `KALLIP_FILES_BLOB_ROOT` | yes (service) | _(unset)_ | Root directory of the content-addressed blob store; created on demand. |
| `KALLIP_FILES_DATABASE_URL` | yes (service) | _(unset)_ | Postgres URL for the metadata store; a missing URL fails fast at boot. |
| `KALLIP_FILES_ARCHEION_INTERNAL_URL` | yes (service) | _(unset)_ | Archeion internal base URL for `/internal/*` ControlPlane calls. Must NOT be publicly reachable. |
| `KALLIP_POLIS_INTERNAL_TOKEN_FILE` | yes (service) | _(unset)_ | File holding the platform-internal secret bearer for the archeion `/internal/*` API, provisioned by the archeion (0640 under its state directory) and read at boot by the lesche, files, and instances services. |
| `KALLIP_FILES_MAX_BODY_SIZE_MB` | no | `100` | Maximum accepted upload body, in megabytes; larger streams are cut off with 413. |
| `KALLIP_FILES_DEGRADE` | no | `closed` | Archeion degrade posture: `closed` fails authorization with 503 when the registry cannot answer; `soft` degrades to deny (403). Neither posture weakens credential verification. |
| `KALLIP_FILES_GC_INTERVAL_SECS` | no | `60` | Delay between GC passes (sweep + reconcile), in seconds. |
| `KALLIP_FILES_GC_GRACE_SECS` | no | `60` | How long a zero-refcount row must have been freed before the GC may reclaim it. |
| `KALLIP_FILES_GC_BATCH` | no | `128` | Maximum catalog rows reclaimed per GC pass. |
| `KALLIP_FILES_URL` | yes (CLI) | _(unset)_ | Files service base URL: consumed by the `kallip file` CLI (spawn env) and by the tagma process for record media fetches (plain configuration, not a secret). |
| `KALLIP_FILES_TOKEN` | yes (CLI) | _(unset)_ | Bearer for the `kallip file` CLI (`sk-tagma-…`). Not a tagma setting: the tagma authenticates with its registered enrollment credential and removes a leftover `KALLIP_FILES_TOKEN` from its own environment at boot. |

Source:
[`crates/platform/kallip-files/src/args.rs`](../../crates/platform/kallip-files/src/args.rs).

## System environment variables

The shell backend reads these from the process environment and passes them into
every spawned `bash`:

| Variable | Fallback     | Purpose              |
| -------- | ------------ | -------------------- |
| `HOME`   | _(required)_ | User home directory. |
| `PATH`   | _(required)_ | System PATH.         |

The backend also hardcodes `TERM=dumb`, `NO_COLOR=1`, `LS_COLORS=""`,
`CLICOLOR="0"` into every spawned `bash` to suppress color output.

## Local daemon

Variables read by `kallip-daemon` itself; `kallipctl` mirrors the state
and socket resolution.

| Variable                  | Required | Default                         | Description                                                                                                                                                                                             |
| ------------------------- | -------- | ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_DAEMON_RECORD_DIR` | no | `~/.local/state/kallipai/daemon/instances/` | The daemon's registration-record root: one `<slug>.json` record per managed instance (instance id, owning uid, target uid, workspace, user env, launch anchor, and the pointer at the instance data directory). Set verbatim; overrides the XDG state-home derivation. |
| `KALLIP_TAGMA_SLUG`             | yes (tagma) | —                             | Names the instance; the tagma derives its data root (`<data home>/kallipai/tagmata/<slug>`), config root, and logs from it, and refuses to boot without it. Slug grammar: `[a-z0-9][a-z0-9-]*`, at most 64 characters. Injected by the daemon for managed instances; set it for direct runs (compose sets `main`).                                |
| `KALLIP_TAGMA_ACCEPT_UNSAFE_RUN_AS_ROOT` | no | unset | The tagma real-root guard's only escape. A tagma booting with euid 0 in the initial user namespace (the host's real root; a mapped sandbox root sits in a nested namespace and is unaffected) refuses to start. Only the exact value `1` unlocks the boot, with a warning logged; every other value (empty, `0`, anything else) keeps the refusal. Meant for rootful containers without user-namespace remapping — elsewhere, prefer a dedicated unprivileged user. |
| `KALLIP_HARVEST_BASH` | no | `/bin/bash` | Bash used for the login-environment harvest (both launch forms; on drop-to launches it runs as the target user). An administrative constant: deployments without `/bin/bash` (NixOS) point it at a managed bash; nothing a request or a user environment supplies can move it. |
| `KALLIP_DAEMON_SOCKET_GROUP` | no | — | Control-socket access group: when set, the daemon hands the socket to this group (resolved through the group database at bind time) and widens the mode to 0660, so group members can drive the daemon. |

### Control-socket resolution order

The daemon and its clients (`kallipctl`, the instances service) share one
candidate list, walked in the same order on both sides:

1. `--socket` (`kallipctl` only — the daemon has no flag)
2. `KALLIP_DAEMON_SOCKET`
3. `$XDG_RUNTIME_DIR/kallipai/daemon/control.sock`, when the runtime dir is
   set and usable (login sessions)
4. `~/.local/state/kallipai/daemon/control.sock` (the state-home default)

The daemon binds the first candidate and never falls through on bind
failure. Clients probe the candidates in order and connect to the first
that answers — identical ordering plus sequential probing keeps the
daemon and its clients converged across session types. The socket
file is chmod 0600 by default: filesystem permission is the only
auth. When `KALLIP_DAEMON_SOCKET_GROUP` is set, the daemon hands the
socket to that group and widens the mode to 0660 at bind time, so a
declared group becomes the access boundary (the system form runs the
daemon as root and gates the socket and the nix daemon through one
group — see the NixOS module in `nix/nixos-modules.nix`).
`kallipctl start <slug> -e KEY=VALUE` relaunches a stopped instance with a
one-shot env overlay (same allowlist as spawn: KALLIP_*, RUST_LOG, PATH);
the overlay is never written to the record, so the next
start returns to the recorded env.

### Instance identity and the record area

The daemon keeps one registration record per managed instance:
`<state root>/kallipai/daemon/instances/<slug>.json` (override with
`KALLIP_DAEMON_RECORD_DIR`). The record carries the instance id, the
owning uid, the target uid, the workspace, the user env, the launch
anchor, and the pointer at the instance's data directory
(`~/.local/share/kallipai/tagmata/<slug>`). The data directory itself
carries `runtime.json`, written by the tagma itself (the self-report:
`pid`, `port`, and `starttime` — the kernel start time of the writing
process).

The launch identity is chosen per spawn, never per daemon: without a
`user` on the spawn request the instance runs as the daemon's own user
(the single-user form); with `user` (`kallipctl spawn --user`) the
daemon drops the instance to that pre-declared account and pins its
HOME/XDG environment to the account's home. There is no environment
override for the target — only the request field and the record.

`kallipctl list` classifies each recorded pid:

- `running` — the pid is the launch anchor's exact incarnation
  (anchor starttime verified against `/proc`)
- `stopped` — no runtime.json pid on file
- `dead` — anything else (dead pid, reused pid, unverifiable or mismatched process)

The daemon manages only what it spawned: a manually launched tagma
(booted with `KALLIP_TAGMA_SLUG=<slug>` outside the daemon) publishes
its own `runtime.json` but holds no record, so it stays invisible to
`list`. `stop` verifies the anchor before signaling; `start` refuses a
live instance (slug taken) — stop first, then start re-spawns and
re-anchors.

Source:
[`crates/daemon/kallip-daemon/src/main.rs`](../../crates/daemon/kallip-daemon/src/main.rs).

## Daemon relay fill

The daemon fills these defaults into a spawned tagma's env when the spawn
signals relay intent (any `KALLIP_TAGMA_RELAY_*` entry) but omits the URL;
explicit values in the spawn env always win, a spawn with no relay signal at
all gets nothing, and a variable left unset (or empty) means that URL is not
filled. The fill happens before the record snapshot is written, so a restart
replays the filled env.

| Variable                             | Default                                     | Purpose                                                   |
| ------------------------------------ | ------------------------------------------- | --------------------------------------------------------- |
| `KALLIP_DAEMON_RELAY_ARCHEION_URL`   | `services.kallipai.daemon.relayArchUrl`     | Archeion URL filled into relay-intent spawns that omit it. |
| `KALLIP_DAEMON_RELAY_LESCHE_URL`     | `services.kallipai.daemon.relayLescheUrl`   | Lesche counterpart (tunnel + envelopes).                   |

## Dev stack shape

Three variables drive the dev compose (`compose/dev/polis.nix`) and the web dev
server together (both flow from the root `.env` via direnv):

| Variable        | Default                                     | Purpose                                                                                                                         |
| --------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_EDGE_TLS` | `on` | Edge shape: `on` = Caddy-fronted https+domain topology with the mkcert cert; `off` = plain http (no cert/DNS trust setup; see docs/development.md). |
| `KALLIP_EDGE_PORT` | `443` | Dev edge listener port (compose caddy + the web dev server when non-default); the browser-facing web origin carries it and CORS/oauth derive from it verbatim. |
| `KALLIP_DOMAIN` | `kallipai.com` | The domain the dev server and compose topology derive from (both edge shapes; the plain-http quick start sets `localhost` explicitly); the web app's URLs derive in the browser at runtime. |
