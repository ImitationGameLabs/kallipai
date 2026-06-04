# Daemon HTTP API

The daemon (`just-agent-daemon`) exposes an HTTP API at `JUST_AGENT_DAEMON_ADDR`
(default `127.0.0.1:3000`). Clients — the headless CLI, TUI, or the client
library — connect over HTTP to manage agents, stream events, and handle
approvals.

All endpoints require authentication. For token types, role definitions, and
the full authorization matrix, see [auth.md](auth.md).

## Conventions

- **Base URL**: `http://{JUST_AGENT_DAEMON_ADDR}` (default `127.0.0.1:3000`).
  See [env.md](env.md) for configuration.
- **Authentication**: `Authorization: Bearer <token>` on every request.
  See [auth.md](auth.md).
- **Content-Type**: `application/json` for all request and response bodies.
- **Path parameters**: `{id}` is an agent UUID (`AgentId`), returned by
  `POST /agents`.
- **Error responses**: plain text strings (not JSON-wrapped). For example,
  a `403` returns `"not a superior"`.
- **Timestamps**: RFC 3339 format (e.g. `2025-06-05T14:30:00Z`), except
  `recent_retries.timestamp` which is Unix epoch seconds (`u64`).
- **Empty responses**: endpoints that return no body use the corresponding
  status code alone (`204 No Content`, `202 Accepted`).

## Endpoint Overview

| Method   | Path                                          | Purpose                                  | Auth                    |
| -------- | --------------------------------------------- | ---------------------------------------- | ----------------------- |
| `POST`   | `/agents`                                     | Create a new agent instance              | operator / supervisor   |
| `GET`    | `/agents`                                     | List all running agents                  | any                     |
| `DELETE` | `/agents/{id}`                                | Stop and remove an agent                 | operator / superior     |
| `POST`   | `/agents/{id}/interrupt`                      | Interrupt current agent operation        | operator / superior     |
| `POST`   | `/agents/{id}/message`                        | Send a message                           | any (peer-to-peer)      |
| `GET`    | `/agents/{id}/events`                         | Subscribe to agent events (SSE)          | any                     |
| `GET`    | `/agents/{id}/status`                         | Get context usage and retry history      | any                     |
| `GET`    | `/agents/{id}/permissions`                    | Get permission profile and tool policy   | any                     |
| `GET`    | `/agents/{id}/policy`                         | Get tool policy                          | any                     |
| `PUT`    | `/agents/{id}/policy`                         | Update tool policy                       | operator / superior     |
| `GET`    | `/approvals`                                  | List approvals                           | any (filtered by scope) |
| `GET`    | `/approvals/{id}`                             | Get a single approval                    | operator / superior     |
| `POST`   | `/approvals/{id}`                             | Approve or deny an approval              | operator / superior     |
| `GET`    | `/agents/{id}/skills/paths`                   | Get skill directory paths                | any                     |
| `GET`    | `/agents/{id}/skills/{name}/meta`             | Get skill metadata                       | any                     |
| `POST`   | `/agents/{id}/skills/{name}/promote-request`  | Submit a skill promote request           | self / operator         |
| `GET`    | `/skill-promote-requests`                     | List promote requests                    | any                     |
| `GET`    | `/skill-promote-requests/{id}`                | Show promote request with content diff   | any                     |
| `POST`   | `/skill-promote-requests/{id}`                | Approve or deny a promote request        | operator / root agent   |

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
  "skills": ["string — skill paths relative to skills root (e.g. \"code/refactoring\")"],
  "prompt": "string — initial prompt (optional)",
  "created_by": "AgentId — supervisor ID; omit for root agents (optional)"
}
```

**Response**

```json
{
  "id": "AgentId"
}
```

Status: `201 Created`

| Code | Condition |
| ---- | --------- |
| 400  | Invalid `workspace_root`, skill loading failure, or invalid skill name |
| 403  | Not operator (root agents); supervisor has no remaining delegation depth; `workspace_root` outside supervisor's workspace |
| 404  | Supervisor agent not found |
| 500  | Session creation failure, agent spawn failure, or supervisor deleted during creation |

> **Subagent constraints:** The supervisor must have remaining delegation depth
> (`max_depth > 0`), and the subagent's `workspace_root` must be within the
> supervisor's workspace. The tool policy is inherited from the supervisor.

### `GET /agents` — List agents

Lists all running agents with their workspace root, state, and supervisor.

Auth: any authenticated identity. Response contains no secrets. See [auth.md](auth.md).

**Response**

```json
{
  "agents": [
    {
      "id": "AgentId",
      "workspace_root": "string",
      "state": "idle | busy",
      "created_by": "AgentId | null"
    }
  ]
}
```

Status: `200 OK`

### `DELETE /agents/{id}` — Delete agent

Stops and removes an agent instance. The agent must be idle and have no active
subagents.

Auth: operator or superior. See [auth.md](auth.md).

Status: `204 No Content`

| Code | Condition |
| ---- | --------- |
| 403  | Not a superior of the target agent |
| 404  | Agent not found |
| 409  | Agent is busy (interrupt it first), or agent has active subagents (delete or interrupt them first) |
| 500  | Agent vanished during deletion |

### `POST /agents/{id}/interrupt` — Interrupt agent

Sends a graceful cancellation signal to the agent. The agent persists its state
and stops processing. Use `DELETE` to remove the agent entirely.

Auth: operator or superior. See [auth.md](auth.md).

Status: `202 Accepted`

| Code | Condition |
| ---- | --------- |
| 403  | Not a superior of the target agent |
| 404  | Agent not found |

### `POST /agents/{id}/message` — Send message

Sends a message to the agent's input queue. The daemon accepts the message
immediately and processes it asynchronously.

Auth: any authenticated identity. Inter-agent communication is peer-to-peer;
no supervisor relationship is required. See [auth.md](auth.md).

**Request body**

```json
{
  "text": "string — the message to send"
}
```

Status: `202 Accepted`

| Code | Condition |
| ---- | --------- |
| 404  | Agent not found |
| 500  | Agent reactivation failure |

> **Reactivation:** If the agent's task has terminated (channel closed), the
> daemon attempts reactivation — respawning the agent from persisted state with
> the message as the initial prompt. Existing context, approvals, and auth
> token are preserved.

### `GET /agents/{id}/events` — Subscribe to event stream

Opens an SSE connection to receive real-time agent events. See
[SSE Event Types](#sse-event-types) for the event format.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**: Server-Sent Events stream (`Content-Type: text/event-stream`).
Each event is a JSON object with a `type` field. Keep-alive is enabled.

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
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
  ]
}
```

- `pinned_items`: per-item breakdown of `[label, estimated_tokens]`.
- `last_prompt_tokens`: exact prompt token count from the last provider
  response; `null` if no LLM call has been made.
- `cumulative_usage`: totals across all LLM calls in the session. Present but
  zeroed if no calls have been made.
- `recent_retries`: last 20 retry records, newest first. Empty if no retries
  have occurred.

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
| 404  | Agent not found |

### `GET /agents/{id}/permissions` — Agent permissions

Returns the agent's permission profile (delegation depth, workspace boundary)
and its effective tool policy.

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
  }
}
```

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
| 404  | Agent not found |

### `GET /agents/{id}/policy` — Get tool policy

Returns the agent's tool policy — the default decision and per-tool overrides.

Auth: any authenticated identity. See [auth.md](auth.md).

**Response**

```json
{
  "default": "ask",
  "tools": {
    "shell_session_list": "allow",
    "shell_session_exec": "classify"
  }
}
```

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
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

| Code | Condition |
| ---- | --------- |
| 403  | Not a superior of the target agent |
| 404  | Agent not found |
| 409  | Policy is less strict than parent, or a child agent's policy would be stricter than the new policy |
| 500  | Parent/child not found, no session directory, or persist failure |

> **Strictness ordering:** `deny > ask > classify > allow`. Changes are
> persisted to disk before the in-memory update.

## Approvals

### `GET /approvals` — List approvals

Lists approval entries across all agents where the caller is a superior. Results
can be filtered and paginated.

Auth: any authenticated identity. Results are filtered — each caller only sees
approvals for agents where they are a superior. See [auth.md](auth.md).

**Query parameters**

| Parameter      | Type     | Default | Description                                                         |
| -------------- | -------- | ------- | ------------------------------------------------------------------- |
| `offset`       | `u64`    | `0`     | Number of items to skip                                             |
| `limit`        | `u64`    | `5`     | Page size, clamped to `[1, 20]`                                    |
| `requested_by` | `AgentId` | —       | Filter to approvals from a specific agent                           |
| `status`       | `string` | —       | Filter by status: `committed`, `approved`, `denied`, `redeemed`, `cancelled` |
| `order`        | `string` | `desc`  | Sort order by `created_at`: `asc` or `desc`                        |

**Response**

```json
{
  "items": [
    {
      "id": "string",
      "requested_by": "AgentId",
      "content": {
        "tool_name": "string",
        "arguments": { }
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

| Code | Condition |
| ---- | --------- |
| 400  | Invalid `offset` value |

> **Visibility:** `pending` approvals are never visible — only `committed` and
> later statuses are returned.

### `GET /approvals/{id}` — Get approval

Returns a single approval entry by ID.

Auth: operator or superior of the owning agent. See [auth.md](auth.md).

**Response**: same as a single `ApprovalEntry` object from the list response.

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
| 403  | Not a superior of the owning agent |
| 404  | Approval not found |

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

| Code | Condition |
| ---- | --------- |
| 400  | `decision` is not `"approve"` or `"deny"` |
| 403  | Not a superior, or (for approve) caller's own policy does not `allow` the tool |
| 404  | Approval not found |
| 409  | Approval is not in `committed` status |

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
  "local": "/path/to/session/skills | null"
}
```

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
| 404  | Agent not found |

### `GET /agents/{id}/skills/{name}/meta` — Skill metadata

Returns metadata parsed from the skill's YAML frontmatter.

Auth: any authenticated identity. See [auth.md](auth.md).

**Path parameters**

| Parameter | Type     | Description                          |
| --------- | -------- | ------------------------------------ |
| `id`      | `AgentId` | Agent UUID                           |
| `name`    | `string` | Skill path relative to skills root (e.g. `code/refactoring`) |

**Response**

```json
{
  "name": "string — display label from frontmatter",
  "description": "string | null"
}
```

Status: `200 OK`

| Code | Condition |
| ---- | --------- |
| 400  | Invalid skill name |
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

| Parameter | Type     | Description                          |
| --------- | -------- | ------------------------------------ |
| `id`      | `AgentId` | Agent UUID                           |
| `name`    | `string` | Skill path relative to skills root |

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

| Code | Condition |
| ---- | --------- |
| 400  | Invalid skill name, no valid frontmatter, or attempting to promote the `meta` skill |
| 403  | Not the agent itself or the operator |
| 404  | Agent not found, no session directory, or local skill file does not exist |
| 500  | File I/O failure |

> **Notification:** All root agents are notified of the new request via their
> prompt channels.

### `GET /skill-promote-requests` — List promote requests

Lists all promote requests, optionally filtered by status.

Auth: any authenticated identity. See [auth.md](auth.md).

**Query parameters**

| Parameter | Type     | Default | Description                                        |
| --------- | -------- | ------- | -------------------------------------------------- |
| `status`  | `string` | —       | Filter: `pending`, `approved`, or `denied`         |

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

| Code | Condition |
| ---- | --------- |
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

| Code | Condition |
| ---- | --------- |
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

| Code | Condition |
| ---- | --------- |
| 403  | Not the operator or a root agent |
| 404  | Promote request not found |
| 409  | Request is not in `pending` status, or shared skill was modified concurrently (TOCTOU) |
| 500  | File I/O failure |

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

| `type`                    | Fields                   | Description               |
| ------------------------- | ------------------------ | ------------------------- |
| `reasoning`               | `content: string`        | Full reasoning text       |
| `reasoningDelta`          | `delta: string`          | Incremental reasoning chunk |
| `assistantContent`        | `content: string`        | Full assistant content    |
| `assistantContentDelta`   | `delta: string`          | Incremental content chunk |

### Tool execution

| `type`       | Fields                      | Description          |
| ------------ | --------------------------- | -------------------- |
| `toolCall`   | `name: string, args: string` | Tool invocation     |
| `toolResult` | `result: string`            | Tool execution result |

### Terminal events

| `type`              | Fields            | Description                        |
| ------------------- | ----------------- | ---------------------------------- |
| `finished`          | `content: string` | Agent completed successfully       |
| `maxRoundsExceeded` | *(none)*          | Hit the max tool rounds limit      |
| `error`             | `message: string` | Unrecoverable error                |
| `cancelled`         | *(none)*          | Agent was interrupted              |

### State and notifications

| `type`            | Fields                                           | Description                      |
| ----------------- | ------------------------------------------------ | -------------------------------- |
| `busy`            | *(none)*                                         | Agent transitioned to busy state |
| `status`          | `message: string`                                | Informational status message     |
| `approvalUpdated` | `id: string, status: "committed" \| "approved" \| "denied" \| "redeemed" \| "cancelled"` | Approval state changed |
| `retrying`        | `attempt: u32, max_attempts: u32, error: string, delay_secs: f64` | LLM API retry in progress |
