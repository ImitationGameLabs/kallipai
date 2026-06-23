# Architecture

just-agent is a **daemon-centric** agent runtime. Unlike most coding agents
where the UI process _is_ the agent, here the daemon is the long-lived host and
all clients are thin surfaces.

For planned direction, see [roadmap.md](roadmap.md).

The daemon (`just-agent-daemon`) is the center: it hosts multiple isolated agent
instances, each running as a pair of tokio tasks (agent task + bridge task)
behind an HTTP API. Clients — the headless CLI (`just-agent`), the runner
(`just-agent-run`), or the TUI (`just-agent-tui`) — connect over HTTP
and SSE, send messages, stream events,
and disconnect without affecting running agents.

## Why a daemon?

Most coding agents are single-process: the UI hosts the LLM loop directly. This
works for single-session coding but breaks down when you need:

- **Multiple agents** running simultaneously across different projects
- **Agent-to-agent coordination** — one agent spawning and managing others
- **Detached operation** — agents continue running after the client disconnects
- **Multiple client surfaces** — headless CLI for agents, runner for scripting,
  TUI for interactive use, programmatic access via the client library

The daemon makes these possible. Each agent is an isolated unit behind a stable
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
| Stateless shell backend                          | Yes        |

Agents do not share any runtime state. The daemon holds them in a `Vec` behind
an `RwLock`; lookup is by UUID.

### Lifecycle

1. **Create** — `POST /agents` spawns both tasks, returns the agent ID.
2. **Interact** — send messages, stream events, approve or deny pending actions.
3. **Remove** — `DELETE /agents/{id}` aborts both tokio tasks, then moves the
   agent directory to `archived/` (history and usage preserved) and drops the
   registry entry.

The daemon exposes an HTTP API for managing agents and approvals. For the full
endpoint reference, see [daemon-api.md](reference/daemon-api.md). For
authentication and the authorization matrix, see [auth.md](reference/auth.md).

## Request flow

1. Client sends `POST /agents/{id}/message` with the message text.
2. Daemon forwards the text as a `String` to the agent's `mpsc` channel.
3. Agent task receives the input, pushes it as a turn, and calls `run_agent_rounds`.
4. Agent composes context, streams the LLM request, and executes any tool calls.
5. Agent emits `AgentEvent`s (reasoning, content, tool calls, finished) to its
   `mpsc` channel.
6. Bridge task receives `AgentEvent`s, converts them to `SseEvent`s, and
   broadcasts via a `broadcast` channel.
7. Client, subscribed to `GET /agents/{id}/events`, receives the streamed SSE
   events.

## Agent loop

The core loop (`run_agent_rounds` in `just-agent-runtime`) iterates up to
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

Tools go through a three-layer policy before execution:

**Layer 1 — `AgentPolicy`** routes by tool name:

| Tool                       | Decision                   |
| -------------------------- | -------------------------- |
| `bash_background_read`     | Allow (read-only)          |
| `bash_background_kill`     | Allow (agent-spawned task) |
| `bash_exec`                | Delegate to AST classifier |
| Context tools, skill tools | Allow                      |
| Unknown tools              | Ask                        |

**Layer 2 — `AuthorizedToolExecutor`** enforces the decision:

- **Allow** — dispatch immediately.
- **Deny** — return an error to the LLM.
- **Ask** — enqueue in `ApprovalStore`, return a deferred reference. The LLM
  continues working and can redeem later after external approval.

**Layer 3 — Shell command classifier** (`policy/classifier`) is a self-contained
module that parses commands via `rable` and returns its own `Safety` decision
(`ReadOnly` / `NeedsApproval` / `Reject`), which the policy layer maps to
`Allow` / `Ask` / `Deny`. It is fail-closed: unparseable input is `Reject`.

- **Explicit read-only catalog.** A command is auto-approved only if it appears
  in the catalog (`catalog::READ_ONLY_CATALOG`) and satisfies its constraints.
  Anything not listed — including every mutating/dangerous command (`sudo`,
  `dd`, `rm`, …) — defers to approval by default. There is no separate
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
  write/append redirects (`>`, `>>`).

### Approval flow

1. Agent calls a tool that policy classifies as "Ask".
2. `ApprovalStore.enqueue()` stores the call and returns a deferred JSON to the LLM.
3. An `ApprovalUpdated` SSE event is emitted (supervisor-facing).
4. Client sees the event and sends `POST /approvals/{id}` to approve or deny.
5. `ApprovalStore.approve()` or `.deny()` pushes a notification.
6. On the next agent round, the notification is drained into context.
7. The LLM calls `approval_redeem` to execute the stored tool action.

## Crate responsibilities

| Crate                | Role                                                                                          |
| -------------------- | --------------------------------------------------------------------------------------------- |
| `just-agent-common`  | Shared types, slash command definitions, and protocol types. Used by all crates.              |
| `just-agent-runtime` | Agent runtime: agent loop, context management, tool dispatch, policy engine. No network code. |
| `just-agent-shell`   | Provider-neutral shell/session tools for LLM applications. Used by the runtime.               |
| `just-agent-daemon`  | HTTP server hosting agent instances. Uses `just-agent-runtime` internally.                    |
| `just-agent`         | Headless CLI for agents. Thin wrapper over `just-agent-client`. No agent logic.               |
| `just-agent-tui`     | Interactive terminal UI. Same client library, adds ratatui rendering.                         |
| `just-agent-run`     | Agent runner for scripting and automation. Streams progress to stderr, result to stdout.      |
| `just-agent-client`  | Async HTTP client for the daemon API. Used by CLI, TUI, and runner.                           |
