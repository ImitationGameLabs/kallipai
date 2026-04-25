# Architecture

just-agent is a **daemon-centric** agent runtime. Unlike most coding agents
where the UI process *is* the agent, here the daemon is the long-lived host and
all clients are thin surfaces.

The daemon (`just-agent-daemon`) is the center: it hosts multiple isolated agent
instances, each running as a pair of tokio tasks (agent task + bridge task)
behind an HTTP API. Clients — the headless CLI (`just-agent`) or the TUI
(`just-agent-tui`) — connect over HTTP and SSE, send prompts, stream events,
and disconnect without affecting running agents.

## Why a daemon?

Most coding agents are single-process: the UI hosts the LLM loop directly. This
works for single-session coding but breaks down when you need:

- **Multiple agents** running simultaneously across different projects
- **Agent-to-agent coordination** — one agent spawning and managing others
- **Detached operation** — agents continue running after the client disconnects
- **Multiple client surfaces** — CLI for scripting, TUI for interactive use,
  programmatic access via the client library

The daemon makes these possible. Each agent is an isolated unit behind a stable
HTTP API. Clients connect, send prompts, stream events, and disconnect without
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
| `DeferredQueue`                                  | Yes        |
| `AgentConfig` (workspace, skills, system prompt) | Yes        |
| PTY shell backend                                | Yes        |

Agents do not share any runtime state. The daemon holds them in a `Vec` behind
an `RwLock`; lookup is by UUID.

### Lifecycle

1. **Create** — `POST /agents` spawns both tasks, returns the agent ID.
2. **Interact** — send prompts, stream events, approve/reject deferred actions.
3. **Delete** — `DELETE /agents/{id}` aborts both tokio tasks and removes the
   entry.

## HTTP API

| Method   | Path                    | Purpose                                     |
| -------- | ----------------------- | ------------------------------------------- |
| `POST`   | `/agents`               | Create a new agent instance                 |
| `GET`    | `/agents`               | List all running agents                     |
| `DELETE` | `/agents/{id}`          | Kill and remove an agent                    |
| `POST`   | `/agents/{id}/prompt`   | Send a text prompt (returns `202 Accepted`) |
| `GET`    | `/agents/{id}/events`   | Subscribe to agent events via SSE           |
| `POST`   | `/agents/{id}/approval` | Approve or deny a deferred tool call        |
| `GET`    | `/agents/{id}/status`   | Get context usage snapshot                  |
| `POST`   | `/agents/{id}/compact`  | Trigger context compaction                  |
| `POST`   | `/agents/{id}/skill`    | Load a skill into a running agent           |

`send_prompt` returns immediately (`202 Accepted`). Actual processing is async.
Clients subscribe to the SSE endpoint to receive streamed results.

## Request flow

1. Client sends `POST /agents/{id}/prompt` with the prompt text.
2. Daemon forwards the text as `UserInput::Prompt` to the agent's `mpsc` channel.
3. Agent task receives the input, pushes it as a turn, and calls `run_agent_rounds`.
4. Agent composes context, streams the LLM request, and executes any tool calls.
5. Agent emits `AgentEvent`s (reasoning, content, tool calls, finished) to its
   `mpsc` channel.
6. Bridge task receives `AgentEvent`s, converts them to `SseEvent`s, and
   broadcasts via a `broadcast` channel.
7. Client, subscribed to `GET /agents/{id}/events`, receives the streamed SSE
   events.

## Agent loop

The core loop (`run_agent_rounds` in `just-agent-core`) iterates up to
`max_tool_rounds` (default 32) per prompt:

1. Drain deferred notifications into context as a synthetic user message.
2. Compose context from layers (pinned → summary → working turns).
3. Check token budget — if over limit, trigger compaction.
4. Stream the LLM request with tool definitions.
5. If the response has tool calls, execute each through the policy gate.
6. Push the assistant message and tool results as a new turn.
7. Repeat until no tool calls remain (finished) or max rounds exceeded.

## Policy and deferred approval

Tools go through a three-layer policy before execution:

**Layer 1 — `AgentPolicy`** routes by tool name:

| Tool                                          | Decision                              |
| --------------------------------------------- | ------------------------------------- |
| `shell_session_list`, `shell_session_capture` | Allow (read-only)                     |
| `shell_session_create`                        | Allow if cwd is within workspace root |
| `shell_session_exec`                          | Delegate to AST classifier            |
| Context tools, skill tools                    | Allow                                 |
| Unknown tools                                 | Ask                                   |

**Layer 2 — `AuthorizedToolExecutor`** enforces the decision:

- **Allow** — dispatch immediately.
- **Deny** — return an error to the LLM.
- **Ask** — enqueue in `DeferredQueue`, return a deferred reference. The LLM
  continues working and can redeem later after external approval.

**Layer 3 — Shell command classifier** uses AST parsing (via `rable`) to
analyze shell commands:

- Recognizes dangerous commands (`sudo`, `dd`, `mkfs`, `rm -rf`, etc.)
- Detects shell delegation (`bash -c`, `sh -c`) and re-parses inner commands
- Handles pipelines, redirects, and variable assignments
- Maintains an allowlist for read-only commands (`ls`, `cat`, `grep`, `find`,
  etc.)

### Deferred approval flow

1. Agent calls a tool that policy classifies as "Ask".
2. `DeferredQueue.enqueue()` stores the call and returns a deferred JSON to the LLM.
3. A `DeferredCreated` event is emitted via SSE.
4. Client sees the event and sends `POST /agents/{id}/approval` to approve or deny.
5. `DeferredQueue.approve()` or `.deny()` pushes a notification.
6. On the next agent round, the notification is drained into context.
7. The LLM calls `approval_redeem` to execute the stored tool action.

## Crate responsibilities

| Crate               | Role                                                                                            |
| ------------------- | ----------------------------------------------------------------------------------------------- |
| `just-agent-core`   | Agent runtime: session loop, context management, tool dispatch, policy engine. No network code. |
| `just-agent-daemon` | HTTP server hosting agent instances. Uses `just-agent-core` internally.                         |
| `just-agent`        | Headless CLI binary. Thin wrapper over `just-agent-client`. No agent logic.                     |
| `just-agent-tui`    | Interactive terminal UI. Same client library, adds ratatui rendering.                           |
| `just-agent-client` | Async HTTP client for the daemon API. Used by both CLI and TUI.                                 |
