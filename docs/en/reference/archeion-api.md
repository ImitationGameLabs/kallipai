---
title: Archeion HTTP API
description: Archeion control-plane auth endpoints and the tagma authorization matrix (repo-internal).
order: 45
internal: true
---

## Conventions

- **Edge origin**: externally, archeion is reached through the platform
  edge at `<origin>/v1/archeion/*`; the edge strips the `/v1/<service>`
  prefix before proxying (profile reads under `/v1/users/*` follow the
  same shape).
- **Direct service port**: against the service itself (default port
  7100) routes carry no prefix: the edge path
  `/v1/archeion/auth/admin-login` is `POST /auth/admin-login` directly.
- Archeion endpoints in this page are written in the edge form unless
  stated otherwise; tagma endpoints (the authorization tables below)
  use the tagma's own unprefixed paths.

## Authorization Matrix

### Agent Management

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

#### Permission Class (FS-Access Downgrade)

A subagent spawn (`POST /agents` with `created_by`) requires a
`permission_class` field (`"normal"` / `"guest"`) that explicitly **downgrades**
the child's FS-access class below the supervisor's own granted class. The tagma
is the reference monitor: a value above the supervisor's class is rejected with
`403 Forbidden`: downgrade only, never an escalation. A `normal` root may thus
spawn a read-only `guest` reviewer. This field is subagent-only; the tagma's
own root takes its class at startup from `KALLIP_ROOT_AGENT_PERMISSION_CLASS`
(see [Agent core and shell](../configuration/tagma/agent.md)). The granted class is reported by
`GET /agents/{id}/permissions` (see the tagma API reference).

#### Profile Sets

| Endpoint                       | Operator | Supervisor | Superior | Any agent | Self |
| ------------------------------ | -------- | ---------- | -------- | --------- | ---- |
| `PUT /agents/{id}/profile-set` | Yes      | —          | Yes      | —         | —    |
| `PUT /profiles/default`        | Yes      | —          | —        | —         | —    |
| `DELETE /profiles/sets/{name}` | Yes      | —          | —        | —         | —    |

Rebinding an agent follows the remove/interrupt pattern (any superior); the
config-level endpoints are operator-only.

#### Context and Policy

| Endpoint                       | Operator | Superior | Any agent |
| ------------------------------ | -------- | -------- | --------- |
| `GET /agents/{id}/status`      | Yes      | —        | Yes       |
| `GET /agents/{id}/permissions` | Yes      | —        | Yes       |

Read-only context endpoints are accessible to any authenticated identity. The
classify preset is tagma-global and immutable; per-command `bash_exec` overrides
(`PUT /agents/{id}/exec-policy`) require operator or superior.

#### Approvals

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

#### Admin-Token Login (Local Platform)

The fifth auth ceremony, `POST /v1/archeion/auth/admin-login`, exists for the
local-platform deployment: it exchanges the operator's `sk-admin-` token
for a normal User session on a fixed local account (the username is
hardcoded `admin` via `LOCAL_ADMIN_USERNAME`; the account is created on
first use and bound by an `external_identities (local-admin, admin)`
marker row, so it can never collide with or take over a real signup's
username: `admin` is on the signup reserved list, and a legacy row
holding the name fails fast with a conflict that points at the admin
surface). The session mints through the same `mint_session_row` path as
every other login, so the whole user-scoped surface (profiles, tagma
mint/enroll) works unchanged; the true admin principal keeps
`/v1/archeion/admin` and the CLI. The session also carries instance rights:
`verify-session` reports `local_admin: true` for the marker account,
and the instances service's platform mode admits that session on its
cookie channel (next section), so the admin login IS the instances
credential -- no separate token to configure or paste.

Security boundary, three sentences: only the admin principal may enter
(any other credential is a plain 401); the route is not mounted unless
`KALLIP_ARCHEION_ADMIN_USER_LOGIN` is explicitly set, so the production
default has no such surface at all; and when it is set, an operator-chosen
admin token shorter than 32 chars refuses to boot (the generated 256-bit
token is exempt). Deleting the marker row together with its user row
resets the account -- the next admin-login recreates both. Mounting the
route is an explicit operator act that pre-provisions an operator
account, so `KALLIP_ARCHEION_SIGNUP_ENABLED` does not gate it.

#### Instances Service Auth (Platform Mode)

The instances service mirrors the lesche's dual-channel auth: a bearer
token verifies via `/internal/verify-bearer` (only `Principal::Admin`
passes -- the app and CLI channel), and, absent a bearer, the archeion
session cookie verifies via `/internal/verify-session`, passing only for
the fixed local-admin account's session (`local_admin`; any other user
session is 403, an absent one 401 with code `admin_session_required`).
Cookie-bearing state-changing requests carry the same two-pillar CSRF
defense as the archeion/lesche (`SameSite=Strict` + the mandatory
`X-Requested-With: kallip` marker on non-GETs; bearer requests are
exempt).

#### Username Availability Probe (Public, Pre-Release)

`GET /v1/archeion/auth/username-availability?username=<handle>` tells a signup
form whether a handle may be attempted, before any credential exists.
It is an explicit, full username-enumeration oracle -- accepted
pre-release for the closed beta -- mounted with the same per-IP
rate-limit family as the other unauthenticated reads (mirroring
`GET /v1/users/{username}`'s posture). The probe is always a 200: the
form's debounce loop is a read protocol, so every outcome is a body
status, never an HTTP error path. The body carries the canonical
(normalized) handle and one of four statuses, decided in a fixed
refusal order: `invalid` (shape/length/charset, or a missing param),
`reserved` (the static reserved list -- beats the lookup, so a legacy
row holding the name still reports `reserved`), `taken` (a live or
disabled user row), `available`.
