# Authentication and authorization

## Token types

All endpoints require a `Bearer` token in the `Authorization` header. The daemon
generates two categories of token:

- **Operator token** — printed once at daemon startup. Grants full access: create
  root agents, manage any agent, approve/deny approvals.
- **Agent token** — generated per agent at creation, injected into the PTY as
  `JUST_AGENT_AUTH_TOKEN`. Agents use this to call back to the daemon.

## Roles

- **Supervisor** — the direct parent: the agent whose `created_by` field points to
  the caller.
- **Superior** — any ancestor in the `created_by` chain (supervisor,
  grand-supervisor, etc.).
- **Self** — the agent itself (identity matches the target agent ID).
- **Root agent** — an agent with no `created_by` (top-level, created by the
  operator).

## Authorization matrix

### Agent management

| Endpoint                      | Operator | Supervisor | Superior | Any agent | Self |
| ----------------------------- | -------- | ---------- | -------- | --------- | ---- |
| `POST /agents` (root)         | Yes      | —          | —        | —         | —    |
| `POST /agents` (subagent)     | Yes      | Yes        | —        | —         | —    |
| `GET /agents`                 | Yes      | —          | —        | Yes       | —    |
| `DELETE /agents/{id}`         | Yes      | —          | Yes      | —         | —    |
| `POST /agents/{id}/interrupt` | Yes      | —          | Yes      | —         | —    |
| `POST /agents/{id}/message`   | Yes      | —          | —        | Yes       | —    |
| `GET /agents/{id}/events`     | Yes      | —          | —        | Yes       | —    |

Message and event endpoints are peer-to-peer: any authenticated identity
(including the operator) may communicate with any other agent. Management
endpoints (remove, interrupt)
require a superior relationship. Subagent creation requires the direct
supervisor.

### Context and policy

| Endpoint                       | Operator | Superior | Any agent |
| ------------------------------ | -------- | -------- | --------- |
| `GET /agents/{id}/status`      | Yes      | —        | Yes       |
| `GET /agents/{id}/permissions` | Yes      | —        | Yes       |
| `GET /agents/{id}/policy`      | Yes      | —        | Yes       |
| `PUT /agents/{id}/policy`      | Yes      | Yes      | —         |

Read-only context endpoints are accessible to any authenticated identity.
Policy mutation requires operator or superior.

### Approvals

| Endpoint               | Operator | Superior | Any agent | Notes                                |
| ---------------------- | -------- | -------- | --------- | ------------------------------------ |
| `GET /approvals`       | Yes      | —        | Yes       | Results filtered to superior's scope |
| `GET /approvals/{id}`  | Yes      | Yes      | —         | Must be superior of the owning agent |
| `POST /approvals/{id}` | Yes      | Yes      | —         | Approve has additional policy gate   |

For **approve** decisions, an additional policy gate applies: the caller's own
`ToolPolicy` must set the specific tool to `allow`. This prevents superiors from
using subordinates as proxies to bypass their own tool restrictions. The
operator identity is exempt. **Deny** decisions have no policy gate.

### Skills

| Endpoint                                          | Operator | Any agent | Self |
| ------------------------------------------------- | -------- | --------- | ---- |
| `GET /agents/{id}/skills/paths`                   | Yes      | Yes       | —    |
| `GET /agents/{id}/skills/{name}/meta`             | Yes      | Yes       | —    |
| `POST /agents/{id}/skills/{name}/promote-request` | Yes      | —         | Yes  |

Skill discovery endpoints are open to any authenticated identity. Promote
request submission is restricted to the agent itself or the operator.

### Skill promote requests

| Endpoint                            | Operator | Any agent | Root agent |
| ----------------------------------- | -------- | --------- | ---------- |
| `GET /skill-promote-requests`       | Yes      | Yes       | —          |
| `GET /skill-promote-requests/{id}`  | Yes      | Yes       | —          |
| `POST /skill-promote-requests/{id}` | Yes      | —         | Yes        |

Listing and viewing promote requests is open to any authenticated identity.
Responding (approve/deny) is restricted to the operator or root agents.
