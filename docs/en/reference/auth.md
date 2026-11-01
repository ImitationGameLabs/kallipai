---
title: Authentication and authorization
description: Token types and the authorization model across KallipAI services.
order: 20
---

## Token Types

Every request into KallipAI authenticates with a bearer token. Each token's
prefix names its kind at a glance, so a leaked credential is
self-identifying and easy for secret scanners to flag.

- **Operator token** (`sk-operator-…`): persisted 0600 to the tagma's
  `credentials/operator-token.env` at startup (retrieve it with `kallipctl
  operator-token show <slug>`; boot never prints it, and `operator-token
  reset` prints a freshly minted value at the operator's explicit
  request).
  Grants full control of one tagma: manage any agent, approve or deny
  pending actions. The tagma's single root agent is created at startup and
  never through the API.
- **Agent token** (`sk-agent-…`): issued per agent at creation and injected
  into that agent's shell as `KALLIPAI_AUTH_TOKEN`. Agents use it to call back
  to the tagma; people never handle it.
- **Tagma credential** (`sk-tagma-…`): minted when a tagma enrolls.
  One live record per tagma, carrying a permission set. That set governs its
  files-service calls; its room traffic follows its enrollment state. The owner
  renames it, rewrites the set, or resets it; a reset retires the old hash
  immediately and prints the new secret once.
- **Personal access token** (`sk-pat-…`): minted by a signed-in user, with
  a name, a permission set, and a life of 30, 90, or 365 days (90 by
  default). It reaches the registry and the files service on the routes its
  set names, and no other service accepts it. Revoking keeps the row in the
  list, carrying the time it stopped working.

The platform stores only hashes of token material, never the secrets
themselves.

### Roles

- **Supervisor**: the agent that created the caller: its direct parent.
- **Superior**: any ancestor in the creation chain (supervisor,
  grand-supervisor, and up).
- **Self**: the agent itself (identity matches the target agent).
- **Root agent**: the tagma's single agent with no creator. It is
  tagma-managed (created at startup from configuration) and never created or
  removed through the API.

### Authentication Surfaces

Each surface of the platform authenticates differently:

- **Web app**: people sign up and sign in on the web face; the platform
  issues a browser session that grants the user-scoped surfaces (profiles,
  tagma enrollment and management).
- **Personal access tokens**: minted on the web face for people who want
  a credential of their own outside a browser session; each acts as the
  user it was issued to, on the routes its set names.
- **Tagma management**: the operator token authorizes the tagma's
  management API.
- **Platform administration** (local deployments): the admin token
  (`sk-admin-…`) is pinned by the operator and authorizes platform
  administration such as enrollment and user management; it can be
  exchanged at sign-in for an admin web session.
- **Agent callbacks**: the agent token, injected into each agent's shell,
  authenticates the agent (and CLI commands running inside it) to the
  tagma.

## Permission Sets

A credential carries a set of permission strings, and every scoped route
names the one string it needs. Two services read that list: the registry
(`archeion:*`) and the files service (`files:*`). Six strings exist, a read
and a write for each of three resources:

| String                  | Guards                                        |
| ----------------------- | --------------------------------------------- |
| `archeion:tagma:read`   | list the owner's tagmata, read one             |
| `archeion:tagma:write`  | rename or revoke the owner's tagma             |
| `archeion:account:read` | `GET /me`, the caller's own identity           |
| `files:record:read`     | list, download, and stat stored records         |
| `files:record:write`    | upload and delete records                       |
| `files:delivery:write`  | deliver a record into another principal's inbox |

- The registry checks `archeion:*` on its own routes; the files service
  checks `files:*` on the content routes, ahead of its per-path ACL. A set
  that holds the string a route needs passes; one that does not hears which
  string it lacks.
- Every other service judges identity and room membership, not strings: the
  keryx and the relay refuse a personal token with `403` outright, and a
  tagma's room traffic follows its enrollment state rather than its set.
- `401` means the call carries no usable credential: an unknown hash, a
  passed expiry, a revocation, a disabled owner, or a stored set holding a
  string this build does not know, which cannot be read back as a key.
  `403` means the credential is recognized and this route needs a string it
  does not carry.
- An empty set is a state a tagma's owner can create on purpose: its
  file-service calls are authenticated and then refused one at a time. A tagma
  reaches no registry route whatever its set holds. A personal token is created
  with at least one string (the registry answers `400` otherwise). Ending a
  credential is a revocation or a reset, never an emptied set.
- A change lands on the credential's next request; nothing caches the set.
- A reset is the one place where the next request is not enough on its
  own: the registry retires the old hash at once, and a running tagma holds
  the value it read. Write the new value into its credential file: the
  relay, file, and gateway calls reload once when a request is rejected,
  so the next request runs on the new value. A gateway source that stopped
  on the old credential stops refusing new spawns at its next gate check,
  which asks the store on every call; the pull itself resumes at the next
  pull round (the backoff between rounds tops out at five minutes) or at a
  `POST /profiles/refresh`, so no restart is needed either way.
- A tagma enrollment carries the three `files:*` strings. A new personal
  token starts checked with `archeion:tagma:read`,
  `archeion:account:read`, and `files:record:read`.
- Identities that carry no record are not narrowed: a browser session and
  the admin token pass every string check.

Minting, rescoping, or resetting a credential, passkeys, email addresses,
OAuth links, unlinked identities, device pairing, and logout belong to the
session alone: a personal token is refused there with `403` however many
strings it holds, because a credential that can mint another, or add a way to
sign in, can promote itself.

`POST /tagmata` (an enrollment code) is not on that list. It answers on
`archeion:tagma:write`, so a personal token carrying the string can mint a
pending tagma under its own owner and cap, and the admin token passes the
string check but then stops at the user check, belonging to no user. The two
`403`s are told apart by the error `code`: `perm_missing` when the token lacks
the string, `user_required` when the principal is not a user.
