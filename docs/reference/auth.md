# Authentication and authorization

## Token types

All endpoints require a `Bearer` token in the `Authorization` header. The tagma
generates two categories of token:

- **Operator token** — printed once at tagma startup. Grants full access:
  manage any agent, approve/deny approvals. (The tagma's single root agent is
  tagma-managed — created at startup from env, never created over HTTP.)
- **Agent token** — generated per agent at creation, injected into the agent's shell as
  `KALLIP_AUTH_TOKEN`. Agents use this to call back to the tagma.

Both tokens are 256-bit CSPRNG secrets with a type-tag prefix (`sk-operator-…` /
`sk-agent-…`), so the kind is self-describing and secret scanners can flag leaks. The
tagma **never stores a token in plaintext on long-lived state**: only its SHA-256 hash
is retained (`AppState` and the agent registry index). Incoming bearer tokens are
hashed and compared by hash. Because an attacker cannot steer a SHA-256 output,
variable comparison/lookup time over hashes leaks nothing about the secret — timing is
not a practical vector even off-localhost (e.g. a `0.0.0.0` bind). The single operator
comparison additionally uses a constant-time compare.

## Roles

- **Supervisor** — the direct parent: the agent whose `created_by` field points to
  the caller.
- **Superior** — any ancestor in the `created_by` chain (supervisor,
  grand-supervisor, etc.).
- **Self** — the agent itself (identity matches the target agent ID).
- **Root agent** — the tagma's single agent with no `created_by`. It is
  tagma-managed (eagerly created at startup from env vars), never created or
  removed over HTTP.

## Authorization matrix

### Agent management

| Endpoint                      | Operator | Supervisor | Superior | Any agent | Self |
| ----------------------------- | -------- | ---------- | -------- | --------- | ---- |
| `POST /agents` (subagent)     | Yes      | Yes        | —        | —         | —    |
| `GET /agents`                 | Yes      | —          | —        | Yes       | —    |
| `GET /agents/root`            | Yes      | —          | —        | Yes       | —    |
| `DELETE /agents/{id}`         | Yes      | —          | Yes      | —         | —    |
| `POST /agents/{id}/interrupt` | Yes      | —          | Yes      | —         | —    |
| `POST /agents/{id}/message`   | Yes      | —          | —        | Yes       | —    |
| `GET /agents/{id}/events`     | Yes      | —          | —        | Yes       | —    |
| `PUT /agents/{id}/metadata`   | Yes      | Yes        | —        | —         | —    |
| `PUT /agents/{id}/activity`   | Yes      | —          | —        | —         | Yes  |

Message and event endpoints are peer-to-peer: any authenticated identity
(including the operator) may communicate with any other agent. Management
endpoints (remove, interrupt)
require a superior relationship. Subagent creation requires the direct
supervisor. Metadata (`role`/`description`) is edited by the **direct
supervisor**; activity is **self**-reported (the agent itself, not its
supervisor).

#### Permission class (FS-access downgrade)

A subagent spawn (`POST /agents` with `created_by`) accepts an optional
`permission_class` field (`"normal"` / `"guest"`) that explicitly **downgrades**
the child's FS-access class below its model tier's ceiling. The tagma is the
reference monitor: a value above the tier ceiling or the supervisor's own
granted class is rejected with `403 Forbidden` — downgrade only, never an
escalation. A `normal` root may thus spawn a read-only `guest` reviewer. This
field is subagent-only; the tagma's own root takes its class at startup from
`KALLIP_ROOT_AGENT_PERMISSION_CLASS` (see [env.md](env.md)). The granted class is
reported by `GET /agents/{id}/permissions` (see [tagma-api.md](tagma-api.md)).

### Context and policy

| Endpoint                       | Operator | Superior | Any agent |
| ------------------------------ | -------- | -------- | --------- |
| `GET /agents/{id}/status`      | Yes      | —        | Yes       |
| `GET /agents/{id}/permissions` | Yes      | —        | Yes       |

Read-only context endpoints are accessible to any authenticated identity. The
classify preset is tagma-global and immutable; per-command `bash_exec` overrides
(`PUT /agents/{id}/exec-policy`) require operator or superior.

### Approvals

| Endpoint               | Operator | Superior | Any agent | Notes                                |
| ---------------------- | -------- | -------- | --------- | ------------------------------------ |
| `GET /approvals`       | Yes      | —        | Yes       | Results filtered to superior's scope |
| `GET /approvals/{id}`  | Yes      | Yes      | —         | Must be superior of the owning agent |
| `POST /approvals/{id}` | Yes      | Yes      | —         | Approve has additional classify gate |

For **approve** decisions on a deferred `bash_exec`, an additional classify gate
applies: the caller's own classify rule-set (the tagma-global preset plus the
caller's `ExecPolicy` overrides) must classify the command as `allow`. This
prevents superiors from using subordinates as proxies to run a command their own
policy would gate. The operator identity is exempt. **Deny** decisions have no
gate.

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

## Agora / lesche service-to-service boundary

The cloud relay is split into two services: the **agora** (control plane:
identity, WebAuthn, tagma lifecycle, the durable Postgres store) and the
**lesche** (data plane: herald tunnels, app event streams, envelope routing,
presence — all soft-state). The lesche never touches the durable store; it
authenticates requests, resolves tagma metadata, and advances the tunnel-proof
replay guard through a narrow `ControlPlane` trait, reached over the agora's
non-public `/internal/*` HTTP API.

The two services are addressed on their own subdomains (`agora.<d>` /
`lesche.<d>` — e.g. `agora.localhost` / `lesche.localhost` in dev,
`agora.kallipai.com` / `lesche.kallipai.com` in prod). The web app and the herald
talk to each by its own subdomain. The session cookie carries a configurable
`Domain` attribute (`KALLIP_AGORA_SESSION_COOKIE_DOMAIN`, the parent domain) so
the cookie set on login at `agora.<d>` is also sent to `lesche.<d>`; the two
subdomains share a registrable domain (same-site under `SameSite=Strict`), and
each service's CORS allowlist authorizes the web origin with credentials. A
single-origin deploy leaves the cookie host-only (the attribute unset).

That `/internal/*` surface is guarded by a shared-secret bearer
(`KALLIP_AGORA_INTERNAL_TOKEN` on the agora, `KALLIP_LESCHE_AGORA_TOKEN` on the
lesche — the same value). The comparison is constant-time. If the token is
unset on the agora, the `/internal` nest is not mounted at all (the agora runs
standalone, no relay connected). The surface must be network-isolated so only
the lesche can reach it.

**Revocation latency**: the lesche verifies credentials per request against the
agora (no auth cache). Its hot paths are long-lived connections (a herald
tunnel, an app SSE stream) that authenticate once at open and are not
re-verified mid-stream — so revoking a tagma or disabling a user takes effect
on the lesche when the affected connection is next (re)established, not
necessarily the instant the agora row changes. To force immediate
re-verification, drop the connection (the herald reconnects; the app
reconnects). This is the v1 revocation contract; a JWT migration (local
validation, zero per-request RPC) is the future step if tighter coupling is
ever needed.
