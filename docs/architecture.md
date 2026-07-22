# Architecture

kallipai is a **tagma-centric** agent runtime. Unlike most coding agents
where the UI process _is_ the agent, here the tagma is the long-lived host and
all clients are thin surfaces.

For planned direction, see [roadmap.md](roadmap.md).

The tagma (`kallip-tagma`) is the center: it hosts multiple isolated agent
instances, each running as a pair of tokio tasks (agent task + bridge task)
behind an HTTP API. Clients — the headless CLI (`kallip`), the runner
(`kallip-run`), or the TUI (`kallip-tui`) — connect over HTTP
and SSE, send messages, stream events,
and disconnect without affecting running agents.

## Why a tagma?

Most coding agents are single-process: the UI hosts the LLM loop directly. This
works for single-session coding but breaks down when you need:

- **Multiple agents** running simultaneously across different projects
- **Agent-to-agent coordination** — one agent spawning and managing others
- **Detached operation** — agents continue running after the client disconnects
- **Multiple client surfaces** — headless CLI for agents, runner for scripting,
  TUI for interactive use, programmatic access via the client library

The tagma makes these possible. Each agent is an isolated unit behind a stable
HTTP API. Clients connect, send messages, stream events, and disconnect without
affecting running agents.

## Agent instances

Each agent is a pair of tokio tasks with completely isolated state:

| Resource                                         | Per-agent? |
| ------------------------------------------------ | ---------- |
| Tokio `agent_task`                               | Yes        |
| Tokio `bridge_task`                              | Yes        |
| `mpsc` prompt channel                            | Yes        |
| `broadcast` SSE channel                          | Yes        |
| `ContextStore`                                   | Yes        |
| `ApprovalStore`                                  | Yes        |
| `AgentConfig` (workspace, skills, system prompt) | Yes        |
| Shell backend                                    | Yes        |

Agents do not share any runtime state. The tagma holds them in a `Vec` behind
an `RwLock`; lookup is by UUID.

### Lifecycle

1. **Create** — `POST /agents` spawns both tasks, returns the agent ID.
2. **Interact** — send messages, stream events, approve or deny pending actions.
3. **Remove** — `DELETE /agents/{id}` aborts both tokio tasks, then moves the
   agent directory to `archived/` (history and usage preserved) and drops the
   registry entry.

The tagma exposes an HTTP API for managing agents and approvals. For the full
endpoint reference, see [tagma-api.md](reference/tagma-api.md). For
authentication and the authorization matrix, see [auth.md](reference/auth.md).

## Request flow

1. Client sends `POST /agents/{id}/message` with the message text.
2. Tagma forwards the text as a `String` to the agent's `mpsc` channel.
3. Agent task receives the input, pushes it as a turn, and calls `run_agent_rounds`.
4. Agent composes context, streams the LLM request, and executes any tool calls.
5. Agent emits `AgentEvent`s (reasoning, content, tool calls, finished) to its
   `mpsc` channel.
6. Bridge task receives `AgentEvent`s, converts them to `SseEvent`s, and
   broadcasts via a `broadcast` channel.
7. Client, subscribed to `GET /agents/{id}/events`, receives the streamed SSE
   events.

## Agent loop

The core loop (`run_agent_rounds` in `kallip-runtime`) iterates up to
`max_tool_rounds` (default: unlimited, bounded by token budget) per message:

1. Drain interjected messages (queued prompts from other agents) into context.
2. Drain approval notifications into context as a synthetic user message.
3. Compose context from layers (pinned → summary → working turns).
4. Check token budget — if over limit, summarize old turns and evict.
5. Stream the LLM request with tool definitions.
6. If the response has tool calls, execute each through the policy gate.
7. Push the assistant message and tool results as a new turn.
8. Repeat until no tool calls remain (finished) or max rounds exceeded.

## Policy and approval

Only `bash_exec` is gated — it is the arbitrary-execution surface. Every other
tool is the agent's own self-management (context, skills, background tasks,
exec-policy query, approval redemption) with no security surface, so it runs
unconditionally. The `bash_exec` verdict comes from a single preset-aware
classifier; there is no separate per-tool policy map and no intermediate
"safety" type.

**`AgentPolicy`** routes by tool name:

| Tool             | Decision                       |
| ---------------- | ------------------------------ |
| `bash_exec`      | Delegate to the AST classifier |
| Every other tool | Allow (agent self-management)  |

**`AuthorizedToolExecutor`** enforces the decision:

- **Allow** — dispatch immediately.
- **Deny** — return an error to the LLM.
- **Ask** — enqueue in `ApprovalStore`, return a deferred reference. The LLM
  continues working and can redeem later after external approval.

**Shell command classifier** (`policy/classifier`) is a self-contained module
that parses commands via `rable` and returns a final `ToolDecision` (`Allow` /
`Ask{reason}` / `Deny{reason}`) directly — no separate safety type and no
mapping layer. It is fail-closed: unparseable or empty input is `Deny`.

- **Explicit read-only catalog.** A command is auto-approved only if it appears
  in the catalog (`catalog::READ_ONLY_CATALOG`) and satisfies its constraints.
  Anything not listed — including every mutating/dangerous command (`sudo`,
  `dd`, `rm`, …) — defers to approval under `default`. There is no separate
  "dangerous list".
- **Per-command constraints.** Some catalog entries carry constraints: a flag
  that breaks read-only-ness (`find -delete`, `sort -o`, `yq -i`), a predicate
  (`env <cmd>`), or a read-only subcommand allowlist (`git log`/`status`/… are
  read-only; other `git` subcommands defer).
- **Composition is the OR of components.** A list (`&&`/`;`/`||`) or pipeline
  (`|`) is read-only iff every component is. (Safe because the runtime shell is
  a stateless one-shot process.) The background `&` operator is the exception:
  any backgrounded item defers to approval, since the runtime can neither time
  out nor observe it.
- Detects shell delegation (`bash -c`, `sh -c`, `eval`, `exec`, `source`, `.`)
  and re-parses the inner command.
- Flags sensitive environment-variable overrides (`PATH`, `LD_PRELOAD`, …) and
  write/append redirects (`>`, `>>`, `<>`, `&>`, …), except to `/dev/null` (a
  pure sink). fd duplication/close (`2>&1`, `>&-`) and input redirects (`<`,
  `<<<`) open no file for writing and are read-only.

> **Future seam.** If a second tool ever gains a security surface, the gate in
> `AgentPolicy::evaluate` is the place to re-introduce per-tool routing. Today
> the assumption "only `bash_exec` is gated" is structural, not configured.

### Approval flow

1. Agent calls `bash_exec` and the classifier returns `Ask`.
2. `ApprovalStore.enqueue()` stores the call and returns a deferred JSON to the LLM.
3. An `ApprovalUpdated` SSE event is emitted (supervisor-facing).
4. Client sees the event and sends `POST /approvals/{id}` to approve or deny.
5. `ApprovalStore.approve()` or `.deny()` pushes a notification.
6. On the next agent round, the notification is drained into context.
7. The LLM calls `approval_redeem` to execute the stored tool action.

### Classify presets

The classify rule-set is tagma-global, chosen once at startup by the
`KALLIP_POLICY_PRESET` env var (see `docs/reference/env.md`) and immutable for
the tagma's lifetime. Every agent — root and subagent — runs under the same
preset. The preset _is_ the rule bundle (there is no separate "mode" type):

- **`default`** (also when the env var is unset) — strict: catalog commands
  allow, unclassified commands ask, the builtin denylist (`sed`, `awk`, `ed`,
  `ex`) and structural rejects (`curl | sh`, …) deny.
- **`auto`** — the optimized middle: like `default`, but unclassified commands
  allow too. The denylist and structural rejects still deny.
- **`allow-all`** — **debug preset, not for production.** The classifier
  short-circuits to `Allow` for every parseable command, so the denylist and
  structural rejects do not apply.

Per-command `bash_exec` overrides live separately in `ExecPolicy` (per-agent,
runtime-mutable via `PUT /exec-policy`, inherited monotonically). An explicit
override `Deny`/`Ask` is authoritative and not relaxed by the `auto` preset; a
deliberate supervisor decision stays meaningful under every preset.

## Crate responsibilities

| Crate            | Role                                                                                          |
| ---------------- | --------------------------------------------------------------------------------------------- |
| `kallip-common`  | Shared types, slash command definitions, and protocol types. Used by all crates.              |
| `kallip-runtime` | Agent runtime: agent loop, context management, tool dispatch, policy engine. No network code. |
| `kallip-shell`   | Provider-neutral shell/session tools for LLM applications. Used by the runtime.               |
| `kallip-tagma`   | HTTP server hosting agent instances. Uses `kallip-runtime` internally.                        |
| `kallip`         | Headless CLI for agents. Thin wrapper over `kallip-client`. No agent logic.                   |
| `kallip-tui`     | Interactive terminal UI. Same client library, adds ratatui rendering.                         |
| `kallip-run`     | Agent runner for scripting and automation. Streams progress to stderr, result to stdout.      |
| `kallip-client`  | Async HTTP client for the tagma API. Used by CLI, TUI, and runner.                            |
