# Daemon HTTP API

The daemon (`kallip-daemon`) exposes an HTTP API at `KALLIP_DAEMON_ADDR`
(default `127.0.0.1:3000`). Clients — the agent CLI (`kallip`), the runner (`kallip-run`), TUI,
or the client library — connect over HTTP to manage agents, stream events,
and handle
approvals.

All endpoints require authentication. For token types, role definitions, and
the full authorization matrix, see [auth.md](auth.md).

## Conventions

- **Base URL**: `http://{KALLIP_DAEMON_ADDR}` (default `127.0.0.1:3000`).
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

| Method   | Path                                         | Purpose                                | Auth                         |
| -------- | -------------------------------------------- | -------------------------------------- | ---------------------------- |
| `POST`   | `/agents`                                    | Create a new agent instance            | operator / supervisor        |
| `GET`    | `/agents`                                    | List running agents (`?created_by=`)   | any                          |
| `DELETE` | `/agents/{id}`                               | Stop and remove an agent               | operator / superior          |
| `POST`   | `/agents/{id}/interrupt`                     | Interrupt current agent operation      | operator / superior          |
| `POST`   | `/agents/{id}/message`                       | Send a message                         | any (peer-to-peer)           |
| `GET`    | `/agents/{id}/events`                        | Subscribe to agent events (SSE)        | any                          |
| `GET`    | `/agents/{id}/status`                        | Get context usage and retry history    | any                          |
| `GET`    | `/agents/{id}/permissions`                   | Get permission profile and tool policy | any                          |
| `GET`    | `/agents/{id}/policy`                        | Get tool policy                        | any                          |
| `PUT`    | `/agents/{id}/policy`                        | Update tool policy                     | operator / superior          |
| `PUT`    | `/agents/{id}/metadata`                      | Update role / description              | direct supervisor / operator |
| `PUT`    | `/agents/{id}/activity`                      | Report current activity (self)         | self / operator              |
| `GET`    | `/budget`                                    | Get daemon-wide token budget status    | any                          |
| `POST`   | `/budget`                                    | Adjust or set daemon-wide token budget | operator                     |
| `GET`    | `/approvals`                                 | List approvals                         | any (filtered by scope)      |
| `GET`    | `/approvals/{id}`                            | Get a single approval                  | operator / superior          |
| `POST`   | `/approvals/{id}`                            | Approve or deny an approval            | operator / superior          |
| `GET`    | `/agents/{id}/skills/paths`                  | Get skill directory paths              | any                          |
| `GET`    | `/agents/{id}/skills/{name}/meta`            | Get skill metadata                     | any                          |
| `POST`   | `/agents/{id}/skills/{name}/promote-request` | Submit a skill promote request         | self / operator              |
| `GET`    | `/skill-promote-requests`                    | List promote requests                  | any                          |
| `GET`    | `/skill-promote-requests/{id}`               | Show promote request with content diff | any                          |
| `POST`   | `/skill-promote-requests/{id}`               | Approve or deny a promote request      | operator / root agent        |

## Agent Management

### `POST /agents` — Create agent

Creates a new agent instance. When `created_by` is absent, creates a root agent
(requires operator). When present, creates a subagent (caller must be the named
supervisor).

Auth: operator (root agents) or direct supervisor (subagents). See
[auth.md](auth.md).

**Request body**

```json
{
  "workspace_root": "string — filesystem path (optional)",
  "skills": [
    "string — skill paths relative to skills root (e.g. \"code/refactoring\")"
  ],
  "prompt": "string — initial prompt (optional)",
  "created_by": "AgentId — supervisor ID; omit for root agents (optional)",
  "role": "string — short display label, e.g. \"researcher\" (optional; required non-empty for subagents)",
  "description": "string — longer prose, what this agent is for (optional)",
  "max_tool_rounds": "null — use daemon default (see below)",
  "permission_class": "null — grant the tier ceiling (see below)"
}
```

**`role` / `description`** — display metadata, supervisor-owned. A subagent spawn
(`created_by` present) **requires a non-empty `role`** (fleet discipline so a
superior can tell its subagents apart); a root/operator spawn may omit it. Both
default to `""` and are never used as an address — `AgentId` is canonical.
Mutable later via `PUT /agents/{id}/metadata`.

**`max_tool_rounds`** — override the default/env-configured max tool-call rounds for this agent. Omit or `null` to use the daemon default (`KALLIP_MAX_TOOL_ROUNDS` env var, or unlimited). To set an explicit value:

```json
"max_tool_rounds": {"limited": 64}
```

To force unlimited rounds (bounded only by token budget):

```json
"max_tool_rounds": "unlimited"
```

`Limited` values must be > 0; `Limited(0)` returns 400.

**`permission_class`** — optional explicit FS-access permission class for a
subagent spawn, as the lowercase wire spelling (`"normal"` / `"guest"`).
Honored only when `created_by` is present (subagent path); ignored for root
agents, whose class is governed by `KALLIP_ROOT_AGENT_PERMISSION_CLASS`
(see [env.md](env.md)). Omit or `null` to grant the model tier's ceiling
(`ceiling_for_tier`). The daemon treats an explicit value as a **downgrade
only**: a value above the tier ceiling or the supervisor's own granted class
is rejected with `403 Forbidden` (never silently clamped). So a `normal`
(root-tier) agent can spawn a read-only `guest` subagent for review work, but
no agent can escalate a child above its tier. The granted class is observable
on `GET /agents/{id}/permissions`.

> **Token budget:** All agents share a single daemon-wide token budget
> (default: 100M tokens). Use `POST /budget` to adjust at runtime.

**Response**

```json
{
  "id": "AgentId"
}
```

Status: `201 Created`

| Code | Condition                                                                                                                 |
| ---- | ------------------------------------------------------------------------------------------------------------------------- |
| 400  | Invalid `workspace_root`, skill loading failure, invalid skill name, or subagent spawn with an empty `role`               |
| 403  | Not operator (root agents); supervisor has no remaining delegation depth; `workspace_root` outside supervisor's workspace |
| 404  | Supervisor agent not found                                                                                                |
| 503  | Agent limit reached (`KALLIP_MAX_AGENTS`), or supervisor already has max subagents (`KALLIP_MAX_SUBAGENTS`)               |
| 500  | Session creation failure, agent spawn failure, or supervisor removed during creation                                      |

> **Subagent constraints:** The supervisor must have remaining delegation depth
> (`max_depth > 0`), and the subagent's `workspace_root` must be within the
> supervisor's workspace. The tool policy is inherited from the supervisor.
> Each supervisor may have at most `KALLIP_MAX_SUBAGENTS` (default 20) direct subagents.
>
> **Crash recovery:** Restore is exempt from resource limits. After a daemon
> restart, the agent count may temporarily exceed `KALLIP_MAX_AGENTS`. New
> creation requests will return 503 until agents are removed to make room.

### `GET /agents` — List agents

Lists running agents with their workspace root, state, supervisor, and display
metadata (`role`/`description`/`activity`). Optional `?created_by=<AgentId>`
restricts the result to a superior's direct subagents.

Auth: any authenticated identity. Response contains no secrets. See [auth.md](auth.md).

**Query params**

| Param        | Description                                                                                                                                                                                   |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `created_by` | `AgentId` — only list the direct subagents of this superior. Omit to list all. Any string is accepted (AgentId is not UUID-validated); a value that matches no superior yields an empty list. |

**Response**

```json
{
  "agents": [
    {
      "id": "AgentId",
      "workspace_root": "string",
      "state": "idle | busy",
      "created_by": "AgentId | null",
      "role": "string — short display label (omitted when empty)",
      "description": "string — longer prose (omitted when empty)",
      "activity": "string — agent self-reported current activity (omitted when empty / idle)"
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

Stops and removes an agent instance. The agent must be idle and have no active
subagents.

Removal **archives** the agent: its directory is moved to `archived/<id>/`
(history, cumulative usage, and all persisted state preserved) rather than
destroyed. `scan_agents` ignores `archived/`, so an archived agent is absent
from the live registry and is not restored on daemon restart. There is **no
purge mechanism yet** — archived data (which may contain secrets/PII) persists
indefinitely; a purge command/TTL is a tracked pre-production requirement.

Auth: operator or superior. See [auth.md](auth.md).

Status: `204 No Content`

| Code | Condition                                                                                          |
| ---- | -------------------------------------------------------------------------------------------------- |
| 403  | Not a superior of the target agent                                                                 |
| 404  | Agent not found                                                                                    |
| 409  | Agent is busy (interrupt it first), or agent has active subagents (remove or interrupt them first) |
| 500  | Agent vanished during removal                                                                      |

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

### `POST /agents/{id}/message` — Send message

Sends a message to the agent's input queue. The daemon accepts the message
immediately and processes it asynchronously. Returns queue depth feedback so
callers can gauge expected latency.

Auth: any authenticated identity. Inter-agent communication is peer-to-peer;
no supervisor relationship is required. See [auth.md](auth.md).

**Request body**

```json
{
  "text": "string — the message to send"
}
```

**Response**

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
> daemon creates a fresh message channel, pre-queues the incoming message, then
> respawns the agent from persisted state. Existing context, approvals, and auth
> token are preserved. If reactivation fails, the agent remains in a dead state
> and the next message attempt will retry.
>
> **Backpressure:** The message queue has a configurable capacity
> (`KALLIP_PROMPT_QUEUE_SIZE`, default 5). When the queue is full, the
> daemon returns `503` instead of accepting the message. Callers should wait
> and retry.

### `GET /agents/{id}/events` — Subscribe to event stream

Opens an SSE connection to receive real-time agent events. See
[SSE Event Types](#sse-event-types) for the event format.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**: Server-Sent Events stream (`Content-Type: text/event-stream`).
Each event is a JSON object with a `type` field. Keep-alive is enabled.

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

> **Lagged messages:** If the client reads too slowly, lagged messages are
> silently skipped. For high-volume monitoring, consume events promptly.

## Context & Policy

### `GET /agents/{id}/status` — Agent status

Returns the agent's lifecycle state, context usage snapshot, and recent retry
history.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "state": "idle | busy",
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
      "max_attempts": 3,
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
- `token_budget`: daemon-wide cumulative token consumption limit (shared by all agents).
- `token_consumed`: daemon-wide cumulative tokens consumed (shared by all agents).

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `GET /agents/{id}/permissions` — Agent permissions

Returns the agent's permission profile (delegation depth, workspace boundary,
granted permission class) and its effective tool policy.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "max_depth": 3,
  "workspace_root": "/path/to/workspace",
  "created_by": "AgentId | null",
  "tool_policy": {
    "default": "allow | classify | ask | deny",
    "tools": {
      "tool_name": "allow | classify | ask | deny"
    }
  },
  "permission_class": "normal | guest"
}
```

**`permission_class`** — the FS-access permission class actually granted to
this agent (lowercase `"normal"` / `"guest"`): the value the daemon clamped at
spawn and re-validates on restore. Surfaced so an explicit downgrade
(`POST /agents` `permission_class`) is observable.

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `GET /agents/{id}/policy` — Get tool policy

Returns the agent's tool policy — the default decision and per-tool overrides.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "default": "ask",
  "tools": {
    "bash_background_read": "allow",
    "bash_exec": "classify"
  }
}
```

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `PUT /agents/{id}/policy` — Update tool policy

Replaces the agent's tool policy. The new policy must be at least as strict as
the parent's policy (if the agent has a supervisor), and all child agents'
existing policies must still be at least as strict as the new policy.

Auth: operator or superior. See [auth.md](auth.md).

**Request body**

```json
{
  "default": "ask",
  "tools": {
    "tool_name": "allow"
  }
}
```

Status: `204 No Content`

| Code | Condition                                                                                          |
| ---- | -------------------------------------------------------------------------------------------------- |
| 403  | Not a superior of the target agent                                                                 |
| 404  | Agent not found                                                                                    |
| 409  | Policy is less strict than parent, or a child agent's policy would be stricter than the new policy |
| 500  | Parent/child not found, no persistent directory, or persist failure                                |

> **Strictness ordering:** `deny > ask > classify > allow`. Changes are
> persisted to disk before the in-memory update.

### `PUT /agents/{id}/metadata` — Update role / description

Updates the agent's `role` and/or `description` (the supervisor-owned display
metadata). `None`/omitted fields are left unchanged; `Some(value)` sets the
field. `role` is **change-only** — `role: Some(s)` must be non-empty (it cannot
be cleared). `description` may be cleared with `Some("")`. At least one field
must be provided.

Auth: **direct supervisor** or operator (a grandparent may not relabel a
grandchild). See [auth.md](auth.md).

**Request body**

```json
{
  "role": "string — new role (optional; non-empty if present, cannot be cleared)",
  "description": "string — new description (optional; empty string clears it)"
}
```

**Response** — the updated [`AgentSummary`](#get-agents--list-agents):

```json
{
  "id": "AgentId",
  "workspace_root": "string",
  "state": "idle | busy",
  "created_by": "AgentId | null",
  "role": "string",
  "description": "string",
  "activity": "string"
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

**Request body**

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
> classifies as `Allow` under the default policy — same as every other
> `kallip` management command. (The uncommon `ask-all` debug preset gates
> all commands uniformly; activity is no different from `spawn`/`list` there.)

## Token Budget

A single daemon-wide token budget is shared by all agents. The budget resets
to the default (100M tokens) on daemon restart.

### `GET /budget` — Get budget status

Returns the daemon-wide token budget, cumulative consumption, and remaining tokens.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "budget": 100000000,
  "consumed": 23500000,
  "remaining": 76500000
}
```

Status: `200 OK`

### `POST /budget` — Adjust or set budget

Updates the daemon-wide token budget. Exactly one of `set_remaining` or `delta`
must be provided. The change affects all agents immediately.

Auth: operator only. See [auth.md](auth.md).

**Request body (set remaining)**

```json
{
  "set_remaining": 50000000
}
```

The daemon computes `new_total = consumed + set_remaining`. Use `set_remaining: 0`
to pause all agents (remaining = 0 triggers immediate budget exceeded).

**Request body (delta adjustment)**

```json
{
  "delta": 50000000
}
```

Adjusts the total budget by a signed delta. Positive increases, negative
decreases. The new budget must remain above tokens already consumed.

**Response**

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
> the default (100M tokens) on daemon restart.

## Approvals

### `GET /approvals` — List approvals

Lists approval entries across all agents where the caller is a superior. Results
can be filtered and paginated.

Auth: any authenticated identity. Results are filtered — each caller only sees
approvals for agents where they are a superior. See [auth.md](auth.md).

**Query parameters**

| Parameter      | Type      | Default | Description                                                                  |
| -------------- | --------- | ------- | ---------------------------------------------------------------------------- |
| `offset`       | `u64`     | `0`     | Number of items to skip                                                      |
| `limit`        | `u64`     | `5`     | Page size, clamped to `[1, 20]`                                              |
| `requested_by` | `AgentId` | —       | Filter to approvals from a specific agent                                    |
| `status`       | `string`  | —       | Filter by status: `committed`, `approved`, `denied`, `redeemed`, `cancelled` |
| `order`        | `string`  | `desc`  | Sort order by `created_at`: `asc` or `desc`                                  |

**Response**

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

**Response**: same as a single `ApprovalEntry` object from the list response.

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

**Request body**

```json
{
  "decision": "approve | deny",
  "reason": "string — denial reason (optional, defaults to \"denied\")"
}
```

Status: `200 OK`

| Code | Condition                                                                      |
| ---- | ------------------------------------------------------------------------------ |
| 400  | `decision` is not `"approve"` or `"deny"`                                      |
| 403  | Not a superior, or (for approve) caller's own policy does not `allow` the tool |
| 404  | Approval not found                                                             |
| 409  | Approval is not in `committed` status                                          |

> **Policy gate on approve:** Agent callers must have their own `ToolPolicy`
> set to `allow` for the specific tool in question. This prevents superiors
> from using subordinates as proxies to bypass their own tool restrictions.
> The operator identity is exempt from this check. Deny decisions have no
> policy gate.

## Skills

### `GET /agents/{id}/skills/paths` — Skill directory paths

Returns the shared and agent-local skill directory paths.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "shared": "/path/to/shared/skills",
  "local": "/path/to/agent/skills | null"
}
```

Status: `200 OK`

| Code | Condition       |
| ---- | --------------- |
| 404  | Agent not found |

### `GET /agents/{id}/skills/{name}/meta` — Skill metadata

Returns metadata parsed from the skill's YAML frontmatter.

Auth: any authenticated identity. See [auth.md](auth.md).

**Path parameters**

| Parameter | Type      | Description                                                  |
| --------- | --------- | ------------------------------------------------------------ |
| `id`      | `AgentId` | Agent UUID                                                   |
| `name`    | `string`  | Skill path relative to skills root (e.g. `code/refactoring`) |

**Response**

```json
{
  "name": "string — display label from frontmatter",
  "description": "string | null"
}
```

Status: `200 OK`

| Code | Condition                           |
| ---- | ----------------------------------- |
| 400  | Invalid skill name                  |
| 404  | Agent not found, or skill not found |

> **Note:** `name` in the response is the display label from YAML frontmatter,
> not the canonical skill path identifier. The skill's unique identity is its
> path relative to the skills root (e.g. `code/refactoring`).

## Skill Promote Requests

The promote-request system lets any agent submit a skill for review. Root agents
or the operator review and decide. Content is snapshotted at submission time
(TOCTOU protection).

### `POST /agents/{id}/skills/{name}/promote-request` — Submit promote request

Submits the agent's local skill for promotion to the shared directory. No
request body is required — all data is read from the local skill file on disk.

Auth: the agent itself or the operator. See [auth.md](auth.md).

**Path parameters**

| Parameter | Type      | Description                        |
| --------- | --------- | ---------------------------------- |
| `id`      | `AgentId` | Agent UUID                         |
| `name`    | `string`  | Skill path relative to skills root |

**Response**

```json
{
  "request_id": "spr_abc123def456",
  "skill_name": "code/refactoring",
  "status": "pending",
  "has_existing": true
}
```

Status: `201 Created`

| Code | Condition                                                                           |
| ---- | ----------------------------------------------------------------------------------- |
| 400  | Invalid skill name, no valid frontmatter, or attempting to promote the `meta` skill |
| 403  | Not the agent itself or the operator                                                |
| 404  | Agent not found, no persistent directory, or local skill file does not exist        |
| 500  | File I/O failure                                                                    |

> **Notification:** All root agents are notified of the new request via their
> message channels.

### `GET /skill-promote-requests` — List promote requests

Lists all promote requests, optionally filtered by status.

Auth: any authenticated identity. See [auth.md](auth.md).

**Query parameters**

| Parameter | Type     | Default | Description                                |
| --------- | -------- | ------- | ------------------------------------------ |
| `status`  | `string` | —       | Filter: `pending`, `approved`, or `denied` |

**Response**

```json
{
  "items": [
    {
      "id": "spr_abc123",
      "skill_name": "code/refactoring",
      "has_existing": true,
      "requested_by": "AgentId",
      "status": "pending | approved | denied",
      "deny_reason": "string | null",
      "description": "string | null",
      "created_at": "2025-06-05T14:30:00Z",
      "reviewed_at": "2025-06-05T15:00:00Z | null"
    }
  ],
  "total": 1
}
```

Status: `200 OK`

| Code | Condition                     |
| ---- | ----------------------------- |
| 400  | Invalid `status` filter value |

### `GET /skill-promote-requests/{id}` — Show promote request

Returns full details of a promote request, including the old and new content
for diff review.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**: all fields from the list entry, plus `old_content` and `new_content`:

```json
{
  "old_content": "# Old skill content...\n | null",
  "new_content": "# New skill content...\n"
}
```

Status: `200 OK`

| Code | Condition                 |
| ---- | ------------------------- |
| 404  | Promote request not found |

### `POST /skill-promote-requests/{id}` — Respond to promote request

Approves or denies a pending promote request. On approve, the new skill content
is written to the shared directory.

Auth: operator or root agent. See [auth.md](auth.md).

**Request body**

```json
{
  "decision": "approve | deny",
  "reason": "string — optional; used only for deny decisions, ignored on approve"
}
```

Status: `200 OK`

| Code | Condition                                                                              |
| ---- | -------------------------------------------------------------------------------------- |
| 403  | Not the operator or a root agent                                                       |
| 404  | Promote request not found                                                              |
| 409  | Request is not in `pending` status, or shared skill was modified concurrently (TOCTOU) |
| 500  | File I/O failure                                                                       |

> **TOCTOU protection:** On approve, the shared skill file is re-read and
> compared with the snapshotted `old_content` from submission time. If it has
> changed, the approve is rejected with `409` and the requester must resubmit.

## SSE Event Types

The event stream (`GET /agents/{id}/events`) delivers JSON objects via
Server-Sent Events. Each SSE `data` field contains a JSON object with a `type`
field that identifies the event variant.

Example SSE frame:

```
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
**stays alive** and returns to idle — more events will follow on the next prompt. Only
`cancelled` (a lifecycle cancel from remove / daemon shutdown) ends the stream.

| `type`                   | Fields                                                                                                                               | Description                                                                                                                                                                                                                                                                                                                                       |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `finished`               | `content: string`                                                                                                                    | Agent completed the turn successfully                                                                                                                                                                                                                                                                                                             |
| `maxRoundsExceeded`      | _(none)_                                                                                                                             | Hit the max tool rounds limit for this turn                                                                                                                                                                                                                                                                                                       |
| `error`                  | `message: string`                                                                                                                    | Turn failed with an error; agent stays alive                                                                                                                                                                                                                                                                                                      |
| `failoverChainExhausted` | `reason: "noFailoverConfigured" \| "allBackupsExhausted" \| "allCandidatesUnbuildable" \| "allCandidatesInfeasible", detail: string` | Within-tier failover chain exhausted — every profile in the tier is unavailable; `reason` distinguishes the cause (`allCandidatesInfeasible` = every candidate's declared window violated the budget shape — tune `SUMMARY_MAX_TOKENS` / `PINNED_BUDGET_RATIO` or raise the window), `detail` is the original trigger. Agent stays alive and idle |
| `interrupted`            | _(none)_                                                                                                                             | Round aborted via interrupt; agent stays alive and idle                                                                                                                                                                                                                                                                                           |
| `tokenBudgetExceeded`    | `consumed: u64, budget: u64`                                                                                                         | Token budget hit; agent stays idle until the budget is raised                                                                                                                                                                                                                                                                                     |
| `cancelled`              | _(none)_                                                                                                                             | Lifecycle cancel (remove / shutdown) — agent stops, stream ends                                                                                                                                                                                                                                                                                   |

### State and notifications

| `type`            | Fields                                                                                   | Description                                                                                                                        |
| ----------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `busy`            | _(none)_                                                                                 | Agent transitioned to busy state                                                                                                   |
| `status`          | `message: string`                                                                        | Informational status message                                                                                                       |
| `approvalUpdated` | `id: string, status: "committed" \| "approved" \| "denied" \| "redeemed" \| "cancelled"` | Approval state changed                                                                                                             |
| `retrying`        | `attempt: u32, max_attempts: u32, error: string, delay_secs: f64`                        | LLM API retry in progress                                                                                                          |
| `failover`        | `from: string, to: string, reason: string`                                               | Within-tier failover to the next profile (`from`/`to` are profile ids); non-terminal — the agent stays busy and continues the turn |
