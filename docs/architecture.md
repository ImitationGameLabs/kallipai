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

## Online relay and chat history

The tagma optionally participates in the public-internet relay (agora control
plane + lesche data plane) so a user's app can reach it from anywhere and hold
an E2EE conversation. The in-process relay connector (`relay/` module) enrolls
with the agora on first boot, then holds a long-lived lesche tunnel: it
encrypts outbound agent replies into lesche envelopes and decrypts inbound
user messages. See [container.md](reference/container.md) for the deployment
topology and `KALLIP_TAGMA_RELAY_*` in [env.md](reference/env.md) for the
knobs. Unset = pure-local (the lesche message route returns 503).

The connector persists the **authored transcript** of that conversation to a
SQLite store (`<KALLIP_DATA_DIR>/relay/chat_history.sqlite`) — authored messages
only, in arrival order. Runtime signals (busy/idle, turn terminals, errors) are
deliberately not persisted: they are ephemeral operator metadata, logged for
observability but never replayed. This is the source of truth a reconnecting or
freshly-paired device pulls via `TagmaControl::History` (cursor-based:
`after` for incremental catch-up, `before` for scroll-up, or the recent
window for a first-time device), so the user sees what they missed while
offline. It is **plaintext at rest**, consistent with the host-trust model
(the agent's own `history.ndjson` / `ContextStore` are plaintext on the same
host; E2EE protects transit, not the endpoint). Retention is bounded by a TTL
(default 30d) and a runaway-row cap (default 100k); see
`KALLIP_TAGMA_RELAY_HISTORY_*`. The tagma hosts exactly one conversation
today (the id is derived from the tagma id).

The app keeps a per-device IndexedDB cache of already-rendered authored lines
(`kallip-relay` DB) so a refresh restores the conversation instantly and only
asks the tagma for an incremental delta. It is a disposable derived mirror
(re-pulled on demand), also plaintext, cleared on logout.

### Multi-member rooms (plaintext server-readable)

Multi-member rooms are a **plaintext server-readable** surface by design. The
lesche is the room's store of record: it stores and relays the `RoomMessage`
payload opaquely, enforces member access (a non-member gets a uniform 404 on
envelopes, history, and roster), and fans each message to the room's other live
members. `private` means invite-only membership; `public` means open-access
join. This is a deliberate trust-model decision: the server (the operator's
own deployment) is trusted with room content, so regulated/compliance
deployments can audit it. It does **not** extend to the bilateral 1:1 path —
user-device ↔ own-tagma traffic still crosses the relay as AEAD ciphertext
(`kallip-e2ee`), with the relay seeing only routing metadata.

#### Room identity: `MemberId` vs `ParticipantId`

Two newtypes share one derived UUID, on purpose, at different layers:

- **`ParticipantId`** — the cross-transport **conversation-sender** identity. It is the
  `sender` on every live envelope on BOTH transports (the bilateral 1:1 path and rooms), what
  the tagma persists in `chat_history`, and the key of the relay's shared presence registry.
  Lives in `kallip-agora-common` (`ids.rs`, `participant.rs`); in TS, `@kallipai/kallip-common`
  (`chat.ts`, `ids.ts`).
- **`MemberId`** — the **room-domain** identity: how rooms address their members
  (`RoomMember`, `room_members.member_id`, the roster, room-presence fan-out). It wraps a
  `ParticipantId` (same `for_user`/`for_tagma` derivation, byte-for-byte), so the two convert
  freely at the few seams where the room layer meets the shared transport identity (building
  the wire `Envelope.sender`, presence-registry lookups). It exists so room code is
  member-native — `RoomMember { id: MemberId }`, not the near-synonym clash
  `member's ParticipantId`.

A room member IS a participant who belongs to a room; `MemberId` is that participant identity
viewed through the room layer. The wire JSON is unchanged (`MemberId` is
`#[serde(transparent)]`).

On the TS side the ids are unbranded `string`, so there is no `MemberId` alias -- the same
derived string flows through both roles and `participantIdForUser`/`participantIdForTagma`
results double as member ids by value equality.

## External chat-room API (authored vs signal)

The tagma exposes two event surfaces (see [tagma-api.md](reference/tagma-api.md)).
The **internal** stream carries the full rich event vocabulary for the TUI/CLI.
The **external chat-room API** is the frontend's conversation surface. The tagma
projects each internal event into two channels with different destinations:

- **Authored** — conversation content (a complete assistant message). It crosses
  the E2EE envelope on the relayed path and is persisted in `chat_history`
  (replayable on reconnect).
- **Signal** — runtime/operator metadata (busy/idle presence, turn terminals,
  errors). It crosses a plaintext channel, is ephemeral (never persisted, never
  replayed), and is application-logged for observability. busy/idle live here,
  not in the encrypted envelope, so the envelope stays content-only.

The direct (offline) path serves the same external vocabulary with no relay and
no E2EE. Streaming deltas, tool events, retry/failover telemetry, and approvals
stay internal-only — they never reach the frontend.

## Request flow

1. Client sends `POST /agents/{id}/message` with the message text.
2. Tagma forwards the text as a `String` to the agent's `mpsc` channel.
3. Agent task receives the input, pushes it as a turn, and calls `run_agent_rounds`.
4. Agent composes context, streams the LLM request, and executes any tool calls.
5. Agent emits `AgentEvent`s (reasoning, content, tool calls, finished) to its
   `mpsc` channel.
6. Bridge task receives `AgentEvent`s, converts them to `SseEvent`s, and
   broadcasts via a `broadcast` channel.
7. A client subscribed to the internal event stream receives the full rich
   vocabulary (TUI, `kallip`, `kallip-run`). A frontend client subscribes to the
   external chat-room stream instead, where the tagma splits each event into the
   authored + signal channels above (and persists the authored half).

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
8. Repeat until the agent calls the `break` tool.

A bare assistant response (no tool calls, no `break`) does **not** end the run:
the harness records it, injects a heartbeat prompt, and re-enters the loop. Only
`break` parks the agent (emitting an `Idle` event) — so the agent decides when to
yield. A no-progress guardrail (`max_heartbeat_rounds`, default 3) force-idles
after a bounded storm of bare responses. `FailoverChainExhausted` parks with a
timed retry (bounded by `max_transient_retries`); permanent errors, budget
exhaustion, and max-rounds park and surface to the operator. Messages to the
user are decoupled from all of this: the agent addresses the user by running the
`kallip lesche send` CLI through `bash_exec`, not by ending a turn.

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

| Crate            | Role                                                                                                                                                                                                      |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `kallip-common`  | Shared types, slash command definitions, and protocol types. Used by all crates.                                                                                                                          |
| `kallip-runtime` | Agent runtime: agent loop, context management, tool dispatch, policy engine. No network code.                                                                                                             |
| `kallip-shell`   | Provider-neutral shell/session tools for LLM applications. Used by the runtime.                                                                                                                           |
| `kallip-tagma`   | HTTP server hosting agent instances. Uses `kallip-runtime` internally.                                                                                                                                    |
| `kallip`         | Headless CLI for agents. Thin wrapper over `kallip-client`. No agent logic.                                                                                                                               |
| `kallip-tui`     | Interactive terminal UI. Same client library, adds ratatui rendering.                                                                                                                                     |
| `kallip-run`     | Agent runner for scripting and automation. Streams progress to stderr; emits a semantic exit code (and an optional JSON object on stdout with `--json`). Does not print the agent's user-facing messages. |
| `kallip-client`  | Async HTTP client for the tagma API. Used by CLI, TUI, and runner.                                                                                                                                        |
