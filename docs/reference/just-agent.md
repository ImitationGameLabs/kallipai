# `just-agent` Reference

This is the CLI an agent uses to coordinate with other agents and manage its own
subagents and runtime concerns.

All subcommands use `JUST_AGENT_AUTH_TOKEN` (mandatory) and `JUST_AGENT_DAEMON_URL`
(env, default `http://127.0.0.1:3000`).

## Subcommands

### `message` — Send a message to an agent

```bash
just-agent message <ID> <MESSAGE>
```

Sends a message to the agent's input queue. The daemon accepts the message
immediately (202 Accepted) and processes it asynchronously. Poll `status` to
observe results.

```bash
$ just-agent message "$AGENT_ID" "List all TODO comments in src/"
```

### `status` — Show agent context usage

```bash
just-agent status <ID>
```

Prints context token usage and recent retry history for the agent.

### `aide` — Manage direct subagents

```bash
just-agent aide <subcommand> [args]
```

Manage the **current agent's direct subagents**. The acting supervisor is taken
from the `JUST_AGENT_ID` env var, so these commands only make sense inside an
agent context — they error if it is unset. `aide` is the sole management entry
point; spawning, listing, removing, interrupting, and relabeling agents all go
through here.

| Subcommand            | Purpose                                          |
| --------------------- | ------------------------------------------------ |
| `aide spawn`          | Spawn a direct subagent (`--role` required).     |
| `aide list`           | List the current agent's direct subagents.       |
| `aide remove <ID>`    | Remove a direct subagent.                        |
| `aide interrupt <ID>` | Interrupt a direct subagent's current operation. |
| `aide metadata <ID>`  | Update a direct subagent's role/description.     |

Scoping notes (server-enforced):

- `aide metadata` is restricted to the **direct supervisor**
  (`require_direct_supervisor`); a grandparent cannot relabel a grandchild.
- `aide remove` / `aide interrupt` authorize **any ancestor**
  (`require_superior`), so the direct-subagent framing here is a CLI
  convenience, not a server-side restriction.
- `aide spawn` requires a non-empty `--role`; the daemon rejects subagents with
  an empty role.

```bash
$ just-agent aide list
researcher  idle  ws=/projects/frontend
$ just-agent aide spawn --role reviewer --description "reviews PRs"
b4c2d3e5-...
```

### `approval` — Manage approvals

Subcommands for listing, inspecting, and responding to approvals
(tool actions that require supervisor approval before execution).

#### `approval list` — List approvals

```bash
just-agent approval list [--offset <N>] [--limit <N>] [--requested-by <ID>] [--status <STATUS>] [--all] [--reverse]
```

Lists approvals across all agents visible to the authenticated identity.
Default shows committed actions (awaiting approval); use `--all` to see every status or
`--status` to filter by a specific status
(committed, approved, denied, redeemed, cancelled).

```bash
$ just-agent approval list --limit 5 --status committed
```

#### `approval get` — Show approval details

```bash
just-agent approval get <APPROVAL_ID>
```

Shows full details for a single approval.

```bash
$ just-agent approval get "ap_a1b2c3d4..."
```

#### `approval approve` — Approve a committed action

```bash
just-agent approval approve <APPROVAL_ID>
```

Approve a committed approval. The agent will be notified and can redeem the action.

```bash
$ just-agent approval approve "ap_a1b2c3d4..."
```

#### `approval deny` — Deny a committed action

```bash
just-agent approval deny <APPROVAL_ID> [REASON]
```

Deny a committed approval with an optional reason.

```bash
$ just-agent approval deny "ap_a1b2c3d4..." "too risky"
```

## Usage patterns

### Delegate work to a subagent

```bash
# Spawn a subordinate, then send it work and poll its progress
CHILD=$(just-agent aide spawn --role researcher --prompt "explore the codebase")
just-agent message "$CHILD" "Summarize the project structure"
just-agent status "$CHILD"
```

## Multi-agent orchestration

Agents use this CLI to manage their own subagents. A single daemon can host
agents across multiple projects simultaneously.

### Parallel subagents

```bash
# Spawn two subagents for different scopes
FRONTEND=$(just-agent aide spawn --role reviewer --workspace-root /projects/frontend)
BACKEND=$(just-agent aide spawn --role auditor --workspace-root /projects/backend)

# Send work to both
just-agent message "$FRONTEND" "Review the latest changes for performance issues" &
just-agent message "$BACKEND" "Audit dependencies for known vulnerabilities" &

# Wait for both sends to complete
wait
```

### Inspect and control subagents

```bash
# List your direct subagents
just-agent aide list

# Check a subagent's context usage before sending more work
just-agent status $CHILD

# Interrupt a running subagent gracefully (without removing it)
just-agent aide interrupt $CHILD
```

## Environment variables

`JUST_AGENT_AUTH_TOKEN` (required) and `JUST_AGENT_DAEMON_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including LLM provider configuration and agent tuning parameters, see [env.md](env.md).

## Client library

For Rust programs that need more control than the CLI offers, the
`just-agent-client` crate provides the CLI operations as async methods, plus a
few operator/library-only paths (event streaming, root-agent spawn):

```rust
use just_agent_client::DaemonClient;

let client = DaemonClient::builder("http://127.0.0.1:3000")
    .auth_token(token)
    .build();

// Spawn a root agent (operator-only path; the CLI exposes only subagent spawns)
let id = client.spawn(CreateAgentRequest {
    workspace_root: Some("/project".into()),
    skills: vec!["code-review".into()],
    prompt: None,
    created_by: None,
}).await?;

// Send a message (fire-and-forget)
client.post_message(&id, "Review src/main.rs").await?;

// Stream events (CLI exposes status/activity instead), check status, remove
let mut stream = client.event_stream(&id).await?;
let usage = client.agent_status(&id).await?;
client.remove_agent(&id).await?;
```
