---
title: Authentication and authorization
description: Token types and the authorization model across KallipAI services.
order: 20
---

## Token Types

Every request into KallipAI authenticates with a bearer token. Each token's
prefix names its kind at a glance, so a leaked credential is
self-identifying and easy for secret scanners to flag.

- **Operator token** (`sk-operator-…`): printed once when the tagma starts.
  Grants full control of one tagma: manage any agent, approve or deny
  pending actions. The tagma's single root agent is created at startup and
  never through the API.
- **Agent token** (`sk-agent-…`): issued per agent at creation and injected
  into that agent's shell as `KALLIP_AUTH_TOKEN`. Agents use it to call back
  to the tagma; people never handle it.

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
- **Tagma management**: the operator token authorizes the tagma's
  management API.
- **Platform administration** (local deployments): the admin token
  (`sk-admin-…`) is pinned by the operator and authorizes platform
  administration such as enrollment and user management; it can be
  exchanged at sign-in for an admin web session.
- **Agent callbacks**: the agent token, injected into each agent's shell,
  authenticates the agent (and CLI commands running inside it) to the
  tagma.
