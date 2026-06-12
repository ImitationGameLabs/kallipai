# `just-agent` Reference

Headless CLI for agents — no TTY, no interactive prompts, structured output.
This is the CLI that agents use to interact with the daemon.

All subcommands use `JUST_AGENT_AUTH_TOKEN` (mandatory) and `JUST_AGENT_DAEMON_URL`
(env, default `http://127.0.0.1:3000`).

## Subcommands

### `spawn` — Spawn a new agent

```bash
just-agent spawn [--workspace-root <DIR>] [--skill <name>] [--prompt <text>]
```

Creates a new agent instance on the daemon. Prints the agent ID (a UUID) to
stdout and exits.

```bash
$ just-agent spawn --workspace-root /projects/frontend --skill code-review
a3f1b2c4-5678-90ab-cdef-1234567890ab
```

### `send` — Send a message to an agent

```bash
just-agent send <ID> <MESSAGE>
```

Sends a message to the agent's input queue. The daemon accepts the message
immediately (202 Accepted) and processes it asynchronously. Subscribe to
`events` to observe results.

```bash
$ AGENT_ID=$(just-agent spawn --workspace-root /projects/api)
$ just-agent send "$AGENT_ID" "List all TODO comments in src/"
```

### `list` — List running agents

```bash
just-agent list
```

Prints all agents with their workspace root.

### `stop` — Stop an agent

```bash
just-agent stop <ID>
```

Terminates the agent instance on the daemon.

### `events` — Stream agent events

```bash
just-agent events <ID>
```

Streams the agent's SSE event feed to stdout as JSON lines (one event per
line). Useful for monitoring or piping into `jq`.

```bash
$ just-agent events "$AGENT_ID" | jq -c 'select(.type == "ToolCall")'
```

### `status` — Show agent context usage

```bash
just-agent status <ID>
```

Prints context token usage and recent retry history for the agent.

### `interrupt` — Interrupt agent operation

```bash
just-agent interrupt <ID>
```

Gracefully interrupts the agent's current operation. The agent persists its
state and stops processing. Use `stop` to stop the agent entirely.

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

### Send and monitor

```bash
AGENT_ID=$(just-agent spawn --workspace-root /my/project)
just-agent send "$AGENT_ID" "Summarize the project structure"
# Observe results via events stream
just-agent events "$AGENT_ID" | jq -c 'select(.type == "Content")'
```

### Monitor tool calls in real time

```bash
just-agent events "$AGENT_ID" | jq -c '{type, name: .name, args: .args}' &
just-agent send "$AGENT_ID" "Run the test suite and report failures"
```

### Filter for approval requests

```bash
just-agent events "$AGENT_ID" | jq -c 'select(.type == "approvalUpdated" and .status == "committed") |
  {id, status}'
```

## Multi-agent orchestration

Agents use this CLI to manage other agents. A single daemon can
host agents across multiple projects simultaneously.

### Parallel agents across projects

```bash
# Spawn agents for two projects
FRONTEND=$(just-agent spawn --workspace-root /projects/frontend --skill code-review)
BACKEND=$(just-agent spawn --workspace-root /projects/backend --skill security-review)

# Send work to both
just-agent send "$FRONTEND" "Review the latest changes for performance issues" &
just-agent send "$BACKEND" "Audit dependencies for known vulnerabilities" &

# Wait for both sends to complete
wait
```

### Chain agents — coordinate via events

```bash
# Send work to agent A
just-agent send "$AGENT_A" "Create an implementation plan for feature X"

# Agent A's output can be consumed via events, then forwarded to agent B
just-agent send "$AGENT_B" "Review the plan from agent A for security issues"
```

### Cross-project coordination

```bash
# Discover what's running
just-agent list

# Check an agent's context usage before sending more work
just-agent status $AGENT_ID

# Interrupt a running agent gracefully (without killing it)
just-agent interrupt $AGENT_ID
```

## Environment variables

`JUST_AGENT_AUTH_TOKEN` (required) and `JUST_AGENT_DAEMON_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including LLM provider configuration and agent tuning parameters, see [env.md](env.md).

## Client library

For Rust programs that need more control than the CLI offers, the
`just-agent-client` crate provides the same operations as async methods:

```rust
use just_agent_client::DaemonClient;

let client = DaemonClient::builder("http://127.0.0.1:3000")
    .auth_token(token)
    .build();

// Spawn an agent
let id = client.spawn(CreateAgentRequest {
    workspace_root: Some("/project".into()),
    skills: vec!["code-review".into()],
    prompt: None,
    created_by: None,
}).await?;

// Send a message (fire-and-forget)
client.post_message(&id, "Review src/main.rs").await?;

// Stream events, check status, stop
let mut stream = client.event_stream(&id).await?;
let usage = client.agent_status(&id).await?;
client.stop_agent(&id).await?;
```
