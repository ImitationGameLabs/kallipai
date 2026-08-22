# Tagma HTTP API

The tagma (`kallip-tagma`) exposes an HTTP API at `KALLIP_TAGMA_ADDR`
(default `127.0.0.1:3000`). Two event surfaces sit side by side:

- The **internal event stream** (`GET /agents/{id}/events`) carries the full
  rich `SseEvent` vocabulary (streaming deltas, tool events, retry/failover
  telemetry) and is consumed by the agent CLI (`kallip`), the runner
  (`kallip-run`), and the TUI.
- The **external chat-room API** (`GET /agents/{id}/external/events`) is the
  frontend's sole window onto a conversation: a single multiplexed SSE carrying
  authored messages, runtime signals, and status snapshots (see
  [External chat-room API](#external-chat-room-api)).

All endpoints require authentication. For token types, role definitions, and
the full authorization matrix, see [auth.md](auth.md).

## Conventions

- **Base URL**: `http://{KALLIP_TAGMA_ADDR}` (default `127.0.0.1:3000`).
  See [env.md](env.md) for configuration.
- **Authentication**: `Authorization: Bearer <token>` on every request.
  See [auth.md](auth.md).
- **Content-Type**: `application/json` for all request and response bodies.
- **Path parameters**: `{id}` is an agent UUID (`AgentId`), returned by
  `POST /agents`.
- **Error responses**: plain text strings (not JSON-wrapped). For example,
  a `403` returns `"not a superior"`.
- **Body size limit**: any endpoint that accepts a request body may return
  `413 Payload Too Large` when the body exceeds `KALLIP_MAX_BODY_SIZE_KB`
  (default 1024 KB, configurable; `0` = axum built-in 2 MB).
- **Timestamps**: RFC 3339 format (e.g. `2025-06-05T14:30:00Z`), except
  `recent_retries.timestamp` which is Unix epoch seconds (`u64`).
- **Empty responses**: endpoints that return no body use the corresponding
  status code alone (`204 No Content`, `202 Accepted`).

## Endpoint Overview

| Method   | Path                              | Purpose                                    | Auth                         |
| -------- | --------------------------------- | ------------------------------------------ | ---------------------------- |
| `POST`   | `/agents`                         | Create a subagent (`created_by` required)  | supervisor / operator        |
| `GET`    | `/agents`                         | List running agents (`?created_by=`)       | any                          |
| `GET`    | `/agents/root`                    | Fetch the tagma-managed root agent         | any                          |
| `DELETE` | `/agents/{id}`                    | Stop and remove an agent (never the root)  | operator / superior          |
| `POST`   | `/agents/{id}/interrupt`          | Interrupt current agent operation          | operator / superior          |
| `POST`   | `/agents/{id}/wake`               | Kick a parked agent awake                  | operator / superior          |
| `POST`   | `/agents/{id}/message`            | Send a user message (inbound)              | any (peer-to-peer)           |
| `POST`   | `/agents/{id}/lesche/messages`    | Deliver an agent-authored message (root)   | self (root agent)            |
| `GET`    | `/agents/{id}/events`             | Internal event stream (SSE, rich vocab)    | any                          |
| `GET`    | `/agents/{id}/external/events`    | External chat-room stream (SSE, frontend)  | any                          |
| `GET`    | `/agents/{id}/status`             | Get context usage and retry history        | any                          |
| `GET`    | `/agents/{id}/permissions`        | Get permission profile and classify preset | any                          |
| `PUT`    | `/agents/{id}/metadata`           | Update role / description                  | direct supervisor / operator |
| `PUT`    | `/agents/{id}/activity`           | Report current activity (self)             | self / operator              |
| `GET`    | `/budget`                         | Get tagma-wide token budget status         | any                          |
| `POST`   | `/budget`                         | Adjust or set tagma-wide token budget      | operator                     |
| `GET`    | `/approvals`                      | List approvals                             | any (filtered by scope)      |
| `GET`    | `/approvals/{id}`                 | Get a single approval                      | operator / superior          |
| `POST`   | `/approvals/{id}`                 | Approve or deny an approval                | operator / superior          |

## Agent Management

### `POST /agents` — Create subagent

Creates a subagent under an existing supervisor. The request **must** carry
`created_by` (the supervisor id); the caller must be that supervisor (or the
operator).

The tagma's single **root agent is tagma-managed** — eagerly created at
startup from env vars (see `GET /agents/root` and [env.md](env.md)). A request
without `created_by` is rejected with `409 Conflict`; clients never create the
root.

Auth: operator or direct supervisor. See [auth.md](auth.md).

#### Request body

```json
{
  "workspace_root": "string — filesystem path (optional)",
  "skills": [
    "string — skill paths relative to skills root (e.g. \"code/refactoring\")"
  ],
  "prompt": "string — initial prompt (optional)",
  "created_by": "AgentId — supervisor ID (required)",
  "role": "string — short display label, e.g. \"researcher\" (required, non-empty)",
  "description": "string — longer prose, what this agent is for (optional)",
  "max_tool_rounds": "null — use tagma default (see below)",
  "permission_class": "null — grant the tier ceiling (see below)"
}
```

**`role` / `description`** — display metadata, supervisor-owned. A subagent
spawn **requires a non-empty `role`** (fleet discipline so a superior can tell
its subagents apart). Both default to `""` and are never used as an address —
`AgentId` is canonical. Mutable later via `PUT /agents/{id}/metadata`.

**`max_tool_rounds`** — override the default/env-configured max tool-call rounds for this agent. Omit or `null` to use the tagma default (`KALLIP_MAX_TOOL_ROUNDS` env var, or unlimited). To set an explicit value:

```json
"max_tool_rounds": {"limited": 64}
```

To force unlimited rounds (bounded only by token budget):

```json
"max_tool_rounds": "unlimited"
```

`Limited` values must be > 0; `Limited(0)` returns 400.

**`permission_class`** — optional explicit FS-access permission class for the
subagent, as the lowercase wire spelling (`"normal"` / `"guest"`). Omit or
`null` to grant the model tier's ceiling (`ceiling_for_tier`). The tagma
treats an explicit value as a **downgrade only**: a value above the tier
ceiling or the supervisor's own granted class is rejected with `403 Forbidden`
(never silently clamped). So a `normal` (root-tier) agent can spawn a
read-only `guest` subagent for review work, but no agent can escalate a child
above its tier. The granted class is observable on
`GET /agents/{id}/permissions`.

> **Token budget:** All agents share a single tagma-wide token budget
> (default: 100M tokens). Use `POST /budget` to adjust at runtime.

#### Response

```json
{
  "id": "AgentId"
}
```

Status: `201 Created`

| Code | Condition                                                                                                         |
| ---- | ----------------------------------------------------------------------------------------------------------------- |
| 400  | Invalid `workspace_root`, skill loading failure, invalid skill name, or subagent spawn with an empty `role`       |
| 403  | Not the supervisor; supervisor has no remaining delegation depth; `workspace_root` outside supervisor's workspace |
| 404  | Supervisor agent not found                                                                                        |
| 409  | `created_by` absent (the root is tagma-managed; use `GET /agents/root`)                                           |
| 503  | Agent limit reached (`KALLIP_MAX_AGENTS`), or supervisor already has max subagents (`KALLIP_MAX_SUBAGENTS`)       |
| 500  | Session creation failure, agent spawn failure, or supervisor removed during creation                              |

> **Subagent constraints:** The supervisor must have remaining delegation depth
> (`max_depth > 0`), and the subagent's `workspace_root` must be within the
> supervisor's workspace. The per-command `bash_exec` exec-policy is inherited
> from the supervisor; the classify preset is tagma-global (same for every
> agent).
> Each supervisor may have at most `KALLIP_MAX_SUBAGENTS` (default 20) direct subagents.
>
> **Crash recovery:** Restore is exempt from resource limits. After a tagma
> restart, the agent count may temporarily exceed `KALLIP_MAX_AGENTS`. New
> creation requests will return 503 until agents are removed to make room.

### `GET /agents/root` — Fetch the root agent

Returns the tagma's single root agent. The tagma eagerly creates one root at
startup (env-driven; see [env.md](env.md)), so this always succeeds once the
tagma is accepting connections — clients fetch the root here instead of
list-then-create. Any authenticated identity may call it.

**Response:** a single [agent summary](#get-agents--list-agents) object (same
shape as one element of `GET /agents`). Status `200`. A missing root is a
startup-invariant violation surfaced as `500` (never `404`).

### `GET /agents` — List agents

Lists running agents with their workspace root, state, supervisor, and display
metadata (`role`/`description`/`activity`). Optional `?created_by=<AgentId>`
restricts the result to a superior's direct subagents.

Auth: any authenticated identity. Response contains no secrets. See [auth.md](auth.md).

#### Query params

| Param        | Description                                                                                                                                                                                   |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `created_by` | `AgentId` — only list the direct subagents of this superior. Omit to list all. Any string is accepted (AgentId is not UUID-validated); a value that matches no superior yields an empty list. |

#### Response

```json
{
  "agents": [
    {
      "id": "AgentId",
      "workspace_root": "string",
      "state": "idle | busy | waiting | parked | retrying | faulted",
      "created_by": "AgentId | null",
      "role": "string — short display label (omitted when empty)",
      "description": "string — longer prose (omitted when empty)",
      "activity": "string — agent self-reported current activity (omitted when empty / idle)",
      "parked_reason": "object — why the agent parked (present only in state parked)",
      "retrying": "object — the armed transient-retry plan (present only in state retrying)"
    }
  ]
}
```

`role`/`description` are supervisor-owned and persistent; `activity` is ephemeral,
agent-self-reported via `PUT /agents/{id}/activity` (the `kallip activity`
CLI), and cleared on terminal events (empty while idle). All three are omitted
from the JSON when empty.

Status: `200 OK`

### `DELETE /agents/{id}` — Remove agent

Stops and removes an agent instance. Any state except busy is removable (the
lifecycle cancel is honored everywhere, including waiting/retrying outer-loop
parks); the agent must have no active subagents.

Removal **archives** the agent: its directory is moved to `archived/<id>/`
(history, cumulative usage, and all persisted state preserved) rather than
destroyed. `scan_agents` ignores `archived/`, so an archived agent is absent
from the live registry and is not restored on tagma restart. There is **no
purge mechanism yet** — archived data (which may contain secrets/PII) persists
indefinitely; a purge command/TTL is a tracked pre-production requirement.

Auth: operator or superior. See [auth.md](auth.md).

Status: `204 No Content`

| Code | Condition                                                                                                                            |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------ |
| 403  | Not a superior of the target agent                                                                                                   |
| 404  | Agent not found                                                                                                                      |
| 409  | Agent is busy (interrupt it first), has active subagents (remove or interrupt them first), or is the tagma-managed (live) root agent |
| 500  | Agent vanished during removal                                                                                                        |

### `POST /agents/{id}/interrupt` — Interrupt agent

Aborts the agent's current round: the agent stays alive and returns to idle, ready
for the next prompt. If the agent is already idle this is a no-op. Use `DELETE` to
remove the agent entirely.

Auth: operator or superior. See [auth.md](auth.md).

Status: `202 Accepted`

| Code | Condition                          |
| ---- | ---------------------------------- |
| 403  | Not a superior of the target agent |
| 404  | Agent not found                    |

### `POST /agents/{id}/wake` — Wake parked agent

Kicks a parked agent awake: enqueues a `[system]` turn telling the agent why and
how long ago it parked — "you were parked 3m 12s ago: fatal error: boom. Decide
whether to retry, adjust, or report." — and the agent's next round decides what
to do. Only meaningful while the agent is parked.

Auth: operator or superior. See [auth.md](auth.md).

Status: `202 Accepted`

| Code | Condition                                        |
| ---- | ------------------------------------------------ |
| 403  | Not a superior of the target agent               |
| 404  | Agent not found                                  |
| 409  | Agent is not parked, or is faulted               |
| 500  | Parked state without a parked reason (invariant) |

### `POST /agents/{id}/message` — Send message

Sends a message to the agent's input queue. The tagma accepts the message
immediately and processes it asynchronously. Returns queue depth feedback so
callers can gauge expected latency.

Auth: any authenticated identity. Inter-agent communication is peer-to-peer;
no supervisor relationship is required. See [auth.md](auth.md).

#### Request body

```json
{
  "text": "string — the message to send"
}
```

#### Response

```json
{
  "queue_depth": 0,
  "warning": "string | null — present when messages are already queued"
}
```

- `queue_depth == 0`: agent will process the message immediately.
- `queue_depth > 0`: message is queued behind existing messages; `warning`
  includes a human-readable note.

Status: `202 Accepted`

| Code | Condition                                                                        |
| ---- | -------------------------------------------------------------------------------- |
| 404  | Agent not found                                                                  |
| 503  | Message queue is full (`KALLIP_PROMPT_QUEUE_SIZE` messages pending); retry later |
| 500  | Agent reactivation failure                                                       |

> **Reactivation:** If the agent's task has terminated (channel closed), the
> tagma creates a fresh message channel, pre-queues the incoming message, then
> respawns the agent from persisted state. Existing context, approvals, and auth
> token are preserved. If reactivation fails, the agent remains in a dead state
> and the next message attempt will retry.
>
> **Backpressure:** The message queue has a configurable capacity
> (`KALLIP_PROMPT_QUEUE_SIZE`, default 5). When the queue is full, the
> tagma returns `503` instead of accepting the message. Callers should wait
> and retry.

### `GET /agents/{id}/events` — Internal event stream

Opens an SSE connection to receive the full, rich agent event vocabulary
(streaming deltas, tool calls/results, retry/failover telemetry, approvals).
This is the surface the TUI, `kallip`, and `kallip-run` consume; the browser
frontend uses the [external chat-room stream](#external-chat-room-api) instead.
See [SSE Event Types](#sse-event-types) for the event format.

Auth: any authenticated identity. See [auth.md](auth.md).

#### Response

Server-Sent Events stream (`Content-Type: text/event-stream`). Each event is a
JSON object with a `type` field. Keep-alive is enabled.

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

> **Lagged messages:** If the client reads too slowly, lagged messages are
> silently skipped. For high-volume monitoring, consume events promptly.

## External chat-room API

The external chat-room API is the frontend's conversation surface. The tagma's
internal rich vocabulary is projected (`project_external`) into two channels,
each with a different destination and persistence policy:

- **Authored messages** (`AuthoredEvent`) are conversation content. They cross
  the E2EE envelope on the relayed (online) path and are persisted in
  `chat_history`, so a reconnect replays them. Today the only variant is
  `assistant_content` (a complete assistant message — there is no streaming on
  this surface).
- **Runtime signals** (`SignalEvent`) are operator metadata (busy/idle
  presence, turn terminals, errors). They cross a plaintext channel, are
  ephemeral (never persisted, never replayed), and are written to the tagma's
  application log (`tracing`) for observability.

### `GET /agents/{id}/external/events` — External chat-room stream

A single multiplexed SSE carrying the conversation to a frontend client.
Root-only: the direct conversation is the root agent's, so a non-root id is
rejected. The stream discriminates frames by the SSE `event:` field name (not a
JSON `type` field); each frame's `data:` is the JSON payload:

| `event:` name | `data:` payload | Meaning                                                     |
| ------------- | --------------- | ----------------------------------------------------------- |
| `authored`    | `TagmaReply`    | An authored message (ack / op error / replayed user row / `assistant_content`). |
| `signal`      | `SignalEvent`   | A runtime signal (busy/idle/interrupted/cancelled/terminals/error). |
| `status`      | `TagmaStatusPayload` | An aggregate runtime snapshot (root state, subagent counts, token budget). Ephemeral. |

The `TagmaReply` shape mirrors the relay's envelope payload (`kind`-tagged,
snake_case): `event` (authored `assistant_content`), `user_message` (a replayed
inbound row), `message_accepted` (an ack), `error`, `interrupted`, and
`history_batch_end`. The relayed path carries the same payload inside the E2EE
envelope; the direct (offline) path serves it plaintext with no relay and no
E2EE. Status snapshots are pushed on a fixed cadence; the transcript is driven
by signals (busy/idle), not by these snapshots.

Auth: any authenticated identity. See [auth.md](auth.md). Status: `200 OK` (`404`
on a non-root id, `503` if direct serving is not initialized).

### `POST /agents/{id}/lesche/messages` — Deliver an agent-authored message

The root agent's "speak to the user" primitive. The agent invokes the
`kallip lesche send` CLI through `bash_exec`; the tagma synthesizes an
`assistant_content` message, persists it to `chat_history`, and fans it out as
an `authored` frame on the external stream (or, on the relayed path, as an
encrypted envelope). Self-only and root-only: only the root agent may deliver a
user-facing message (an operator announcement is a separate concern).

#### Request body

```json
{ "text": "string — the message to deliver" }
```

Status: `200 OK` (`{ "ok": true }`); `403` (operator or non-root agent);
`429` (burst cap); `502` (delivery failure); `503` (no serving path).

## Context & Policy

### `GET /agents/{id}/status` — Agent status

Returns the agent's lifecycle state, context usage snapshot, and recent retry
history.

Auth: any authenticated identity. See [auth.md](auth.md).

#### Response

```json
{
  "state": "idle | busy | waiting | parked | retrying | faulted",
  "parked_reason": "object — why the agent parked (present only in state parked)",
  "retrying": { "attempt": 2, "max_attempts": 3, "retry_in_secs": 5.0 },
  "context": {
    "pinned_items": [["label", 123]],
    "turn_count": 10,
    "turn_tokens": 5000,
    "last_prompt_tokens": 1234,
    "cumulative_usage": {
      "prompt_tokens": 50000,
      "completion_tokens": 10000,
      "cache_hit_tokens": 30000
    }
  },
  "recent_retries": [
    {
      "timestamp": 1717000000,
      "round": 3,
      "attempt": 2,
      "max_attempts": 10,
      "error": "tool timeout",
      "delay_secs": 2.0
    }
  ],
  "token_budget": 100000000,
  "token_consumed": 23500000,
  "activity": "reading docs/architecture.md"
}
```

- `activity`: ephemeral, agent-self-reported current activity (via
  `PUT /agents/{id}/activity` / `kallip activity`). Empty/omitted while idle
  (cleared on terminal events).
- `pinned_items`: per-item breakdown of `[label, estimated_tokens]`.
- `last_prompt_tokens`: exact prompt token count from the last provider
  response; `null` if no LLM call has been made.
- `cumulative_usage`: totals across all LLM calls for the agent. Present but
  zeroed if no calls have been made.
- `recent_retries`: last 20 retry records, newest first. Empty if no retries
  have occurred.
- `token_budget`: tagma-wide cumulative token consumption limit (shared by all agents).
- `token_consumed`: tagma-wide cumulative tokens consumed (shared by all agents).

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `GET /agents/{id}/permissions` — Agent permissions

Returns the agent's permission profile (delegation depth, workspace boundary,
granted permission class) and the tagma-global `bash_exec` classify preset in
effect.

Auth: any authenticated identity. See [auth.md](auth.md).

#### Response

```json
{
  "max_depth": 3,
  "workspace_root": "/path/to/workspace",
  "created_by": "AgentId | null",
  "preset": "default | auto | allow-all",
  "permission_class": "normal | guest"
}
```

**`preset`** — the tagma-global `bash_exec` classify rule-set in effect for
this agent (read-only; it is set once at tagma startup from
`KALLIP_POLICY_PRESET`). See _Classify presets_ in `docs/architecture.md`.

**`permission_class`** — the FS-access permission class actually granted to
this agent (lowercase `"normal"` / `"guest"`): the value the tagma clamped at
spawn and re-validates on restore. Surfaced so an explicit downgrade
(`POST /agents` `permission_class`) is observable.

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `PUT /agents/{id}/metadata` — Update role / description

Updates the agent's `role` and/or `description` (the supervisor-owned display
metadata). `None`/omitted fields are left unchanged; `Some(value)` sets the
field. `role` is **change-only** — `role: Some(s)` must be non-empty (it cannot
be cleared). `description` may be cleared with `Some("")`. At least one field
must be provided.

Auth: **direct supervisor** or operator (a grandparent may not relabel a
grandchild). See [auth.md](auth.md).

#### Request body

```json
{
  "role": "string — new role (optional; non-empty if present, cannot be cleared)",
  "description": "string — new description (optional; empty string clears it)"
}
```

#### Response

The updated [`AgentSummary`](#get-agents--list-agents):

```json
{
  "id": "AgentId",
  "workspace_root": "string",
  "state": "idle | busy | waiting | parked | retrying | faulted",
  "created_by": "AgentId | null",
  "role": "string",
  "description": "string",
  "activity": "string"
  "parked_reason": "object — present only in state parked",
  "retrying": "object — present only in state retrying"
}
```

Status: `200 OK`

| Code | Condition                                                             |
| ---- | --------------------------------------------------------------------- |
| 400  | `role` provided but empty, or neither `role` nor `description` set    |
| 403  | Caller is not the direct supervisor (or operator) of the target agent |
| 404  | Agent not found                                                       |
| 500  | No on-disk directory, or `meta.json` read/write failure               |

> **Persist ordering & locking:** `meta.json` is rewritten before the in-memory
> `AgentConfig` update, both under one registry write-lock. The lock serializes
> the whole op — necessary because `meta.json` rewrite is a read-modify-write,
> so without it two concurrent PUTs (or a concurrent remove) could lose an
> update. A crash leaves disk as the source of truth and restore self-heals.

### `PUT /agents/{id}/activity` — Report current activity

Sets the agent's ephemeral `activity` (free text, e.g. `"reading docs/x.md"`).
Self-reported: an agent sets **its own** activity via the `kallip activity`
CLI (which reads `KALLIP_ID`); a supervisor observes activity via
[`GET /agents`](#get-agents--list-agents), it does not write it. An empty string
clears it (the bridge also auto-clears on terminal events). Truncated to 256 chars.

Auth: **the agent itself** or operator (`require_self_or_operator`). See [auth.md](auth.md).

#### Request body

```json
{
  "activity": "string — what the agent is doing right now"
}
```

Status: `204 No Content`

| Code | Condition                                    |
| ---- | -------------------------------------------- |
| 403  | Caller is not the target agent (or operator) |
| 404  | Agent not found                              |

> **Policy:** an agent reports activity by running `kallip activity` through
> `bash_exec`. `kallip` is allow-listed in the command classifier, so this
> classifies as `Allow` under every preset — same as every other `kallip`
> management command. See _Classify presets_ in `docs/architecture.md`.

## Token Budget

A single tagma-wide token budget is shared by all agents. The budget resets
to the default (100M tokens) on tagma restart.

### `GET /budget` — Get budget status

Returns the tagma-wide token budget, cumulative consumption, and remaining tokens.

Auth: any authenticated identity. See [auth.md](auth.md).

#### Response

```json
{
  "budget": 100000000,
  "consumed": 23500000,
  "remaining": 76500000
}
```

Status: `200 OK`

### `POST /budget` — Adjust or set budget

Updates the tagma-wide token budget. Exactly one of `set_remaining` or `delta`
must be provided. The change affects all agents immediately.

Auth: operator only. See [auth.md](auth.md).

#### Request body (set remaining)

```json
{
  "set_remaining": 50000000
}
```

The tagma computes `new_total = consumed + set_remaining`. Use `set_remaining: 0`
to pause all agents (remaining = 0 triggers immediate budget exceeded).

#### Request body (delta adjustment)

```json
{
  "delta": 50000000
}
```

Adjusts the total budget by a signed delta. Positive increases, negative
decreases. The new budget must remain above tokens already consumed.

#### Response

```json
{
  "budget": 150000000,
  "consumed": 23500000,
  "remaining": 126500000
}
```

Status: `200 OK`

| Code | Condition                                                                 |
| ---- | ------------------------------------------------------------------------- |
| 400  | Both or neither `set_remaining`/`delta` provided, or `delta` is zero      |
| 403  | Not the operator                                                          |
| 409  | New budget would be at or below tokens already consumed (delta path only) |

> **No persistence:** Budget changes are in-memory only. The budget resets to
> the default (100M tokens) on tagma restart.

## Approvals

> These endpoints and the `approvalUpdated` event are part of the **internal**
> event surface. The external chat-room API does not carry approvals and the
> browser frontend does not render them; these are consumed by the TUI and
> admin/automation clients over the internal event stream
> (`GET /agents/{id}/events`).

### `GET /approvals` — List approvals

Lists approval entries across all agents where the caller is a superior. Results
can be filtered and paginated.

Auth: any authenticated identity. Results are filtered — each caller only sees
approvals for agents where they are a superior. See [auth.md](auth.md).

#### Query parameters

| Parameter      | Type      | Default | Description                                                                  |
| -------------- | --------- | ------- | ---------------------------------------------------------------------------- |
| `offset`       | `u64`     | `0`     | Number of items to skip                                                      |
| `limit`        | `u64`     | `5`     | Page size, clamped to `[1, 20]`                                              |
| `requested_by` | `AgentId` | —       | Filter to approvals from a specific agent                                    |
| `status`       | `string`  | —       | Filter by status: `committed`, `approved`, `denied`, `redeemed`, `cancelled` |
| `order`        | `string`  | `desc`  | Sort order by `created_at`: `asc` or `desc`                                  |

#### Response

```json
{
  "items": [
    {
      "id": "string",
      "requested_by": "AgentId",
      "content": {
        "tool_name": "string",
        "arguments": {}
      },
      "commit_reason": "string | null",
      "status": "committed | approved | denied | redeemed | cancelled",
      "deny_reason": "string | null",
      "created_at": "2025-06-05T14:30:00Z"
    }
  ],
  "total": 42
}
```

Status: `200 OK`

| Code | Condition              |
| ---- | ---------------------- |
| 400  | Invalid `offset` value |

> **Visibility:** `pending` approvals are never visible — only `committed` and
> later statuses are returned.

### `GET /approvals/{id}` — Get approval

Returns a single approval entry by ID.

Auth: operator or superior of the owning agent. See [auth.md](auth.md).

#### Response

Same as a single `ApprovalEntry` object from the list response.

Status: `200 OK`

| Code | Condition                          |
| ---- | ---------------------------------- |
| 403  | Not a superior of the owning agent |
| 404  | Approval not found                 |

### `POST /approvals/{id}` — Respond to approval

Approves or denies a committed approval. On approve, the agent is notified and
can redeem the stored tool action on its next round.

Auth: operator or superior. An additional policy gate applies for approve
decisions — see note below.

#### Request body

```json
{
  "decision": "approve | deny",
  "reason": "string — denial reason (optional, defaults to \"denied\")"
}
```

Status: `200 OK`

| Code | Condition                                                                                                   |
| ---- | ----------------------------------------------------------------------------------------------------------- |
| 400  | `decision` is not `"approve"` or `"deny"`                                                                   |
| 403  | Not a superior, or (for approve of `bash_exec`) the caller's classify rule-set does not `allow` the command |
| 404  | Approval not found                                                                                          |
| 409  | Approval is not in `committed` status                                                                       |

> **Classify gate on approve:** only `bash_exec` can defer (every other tool is
> unconditional `Allow`). When an agent superior approves a deferred
> `bash_exec`, the caller's own classify rule-set (the tagma-global preset plus
> the caller's `ExecPolicy` overrides) must classify the command as `allow`, or
> the approve is rejected with 403. This prevents a superior from using
> subordinates as proxies to run a command its own policy would gate. The
> operator identity is exempt. Deny decisions have no gate.

## SSE Event Types

These are the **internal** event stream's variants (`GET /agents/{id}/events`),
consumed by the TUI, `kallip`, and `kallip-run`. The
[external chat-room stream](#external-chat-room-api) is a separate surface that
does not use this vocabulary -- it discriminates frames by the SSE `event:`
field (`authored` / `signal` / `status`) and carries only complete authored
messages + runtime signals.

The internal event stream delivers JSON objects via Server-Sent Events. Each SSE
`data` field contains a JSON object with a `type` field that identifies the
event variant.

Example SSE frame:

```text
data: {"type":"assistantContentDelta","delta":"Hello, "}
```

### Text streaming

| `type`                  | Fields            | Description                 |
| ----------------------- | ----------------- | --------------------------- |
| `reasoning`             | `content: string` | Full reasoning text         |
| `reasoningDelta`        | `delta: string`   | Incremental reasoning chunk |
| `assistantContent`      | `content: string` | Full assistant content      |
| `assistantContentDelta` | `delta: string`   | Incremental content chunk   |

### Tool execution

| `type`       | Fields                       | Description           |
| ------------ | ---------------------------- | --------------------- |
| `toolCall`   | `name: string, args: string` | Tool invocation       |
| `toolResult` | `result: string`             | Tool execution result |

### Round-outcome events

These signal the end of the current assistant turn. Except for `cancelled`, the agent
**stays alive** — more events will follow after the next wake. The post-turn state
varies: `idle`/`interrupted` return to idle, `waiting` parks on a wake timer,
`tokenBudgetExceeded` re-arms the wait timer as a recovery probe, an armed
`failoverChainExhausted` enters a retrying backoff, and the unarmed/error/max-rounds
outcomes park the agent (kickable via `POST /agents/{id}/wake`). Only `cancelled`
(a lifecycle cancel from remove / tagma shutdown) ends the stream.

| `type`                   | Fields                                                                                                                               | Description                                                                                                                                                                                                                                                                                                                                       |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `idle`                   | _(none)_                                                                                                                              | Agent completed the turn and returned to idle (a `finished`-style content event is `assistantContent`)                                                                                              |
| `maxRoundsExceeded`      | _(none)_                                                                                                                             | Hit the max tool rounds limit for this turn; the agent parks                                                                                                                                              |
| `error`                  | `message: string`                                                                                                                    | Turn failed with a fatal error; the agent parks (kickable via `POST /agents/{id}/wake`)                                                                                                                   |
| `failoverChainExhausted` | `reason: "noFailoverConfigured" \| "allBackupsExhausted" \| "allCandidatesUnbuildable" \| "allCandidatesInfeasible", detail: string, transient_retry: { attempt, max_attempts, retry_in_secs }` | Within-tier failover chain exhausted — every profile in the tier is unavailable; `reason` distinguishes the cause (`allCandidatesInfeasible` = every candidate's declared window violated the budget shape — tune `SUMMARY_MAX_TOKENS` / `PINNED_BUDGET_RATIO` or raise the window), `detail` is the original trigger. With `transient_retry` present the agent enters a retrying backoff (the timer re-runs the original prompt); absent, it parks |
| `waiting`                | `timeout_secs: u64`                                                                                                                  | Turn ended on `break(wait)`; the agent parks on a wake timer — the timer expiring or any external event resumes it                                                                                         |
| `interrupted`            | _(none)_                                                                                                                             | Round aborted via interrupt; agent stays alive and idle                                                                                                                                                                                                                                                                                           |
| `tokenBudgetExceeded`    | `consumed: u64, budget: u64`                                                                                                         | Token budget hit; the agent parks on a re-armed wait timer as a zero-cost recovery probe (waiting, not idle) until the budget is raised                                                                                                 |
| `cancelled`              | _(none)_                                                                                                                             | Lifecycle cancel (remove / shutdown) — agent stops, stream ends                                                                                                                                                                                                                                                                                   |

### State and notifications

| `type`            | Fields                                                                                   | Description                                                                                                                        |
| ----------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `busy`            | _(none)_                                                                                 | Agent transitioned to busy state                                                                                                   |
| `status`          | `message: string`                                                                        | Informational status message                                                                                                       |
| `approvalUpdated` | `id: string, status: "committed" \| "approved" \| "denied" \| "redeemed" \| "cancelled"` | Approval state changed                                                                                                             |
| `retrying`        | `attempt: u32, max_attempts: u32, error: string, delay_secs: f64`                        | LLM API retry in progress                                                                                                          |
| `failover`        | `from: string, to: string, reason: string`                                               | Within-tier failover to the next profile (`from`/`to` are profile ids); non-terminal — the agent stays busy and continues the turn |
