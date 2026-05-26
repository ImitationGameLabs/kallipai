# Headless CLI Reference

`just-agent` is the headless CLI binary. It is designed for scripting and
automation — no TTY, no interactive prompts, structured output. This is how
agents manage other agents.

All subcommands accept `--daemon-url` (env: `JUST_AGENT_DAEMON_URL`, default
`http://localhost:3000`).

## Subcommands

### `start` — Spawn a new agent

```bash
just-agent start [--workspace-root <DIR>] [--skill <name>] [--prompt <text>]
```

Creates a new agent instance on the daemon. Prints the agent ID (a UUID) to
stdout and exits.

```bash
$ just-agent start --workspace-root /projects/frontend --skill code-review
a3f1b2c4-5678-90ab-cdef-1234567890ab
```

### `send` — Send a prompt and wait for result

```bash
just-agent send <ID> <PROMPT> [--timeout <secs>]
```

Sends a prompt to the agent and blocks until the agent finishes, errors, or
times out. Prints the final assistant content to stdout.

```bash
$ AGENT_ID=$(just-agent start --workspace-root /projects/api)
$ just-agent send "$AGENT_ID" "List all TODO comments in src/" --timeout 120
```

Default timeout: 300 seconds.

### `list` — List running agents

```bash
just-agent list
```

Prints all agents with their workspace root.

### `stop` — Kill an agent

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
state and stops processing. Use `stop` to kill the agent entirely.

### `approve` — Respond to a deferred action

```bash
just-agent approve <ID> <REQUEST_ID> <DECISION>
```

Approve or deny a deferred tool call that is awaiting human approval.
`DECISION` is `approve` or `deny`.

```bash
$ just-agent approve "$AGENT_ID" "req-abc123" approve
```

## Scripting patterns

### Capture output in a variable

```bash
AGENT_ID=$(just-agent start --workspace-root /my/project)
RESULT=$(just-agent send "$AGENT_ID" "Summarize the project structure")
echo "$RESULT"
just-agent stop "$AGENT_ID"
```

### Monitor tool calls in real time

```bash
just-agent events "$AGENT_ID" | jq -c '{type, name: .name, args: .args}' &
just-agent send "$AGENT_ID" "Run the test suite and report failures" --timeout 180
```

### Filter for approval requests

```bash
just-agent events "$AGENT_ID" | jq -c 'select(.type == "DeferredCreated") |
  {request_id, tool_name, summary, dangerous}'
```

## Multi-agent orchestration

The headless CLI enables an agent to manage other agents. A single daemon can
host agents across multiple projects simultaneously.

### Parallel agents across projects

```bash
# Spawn agents for two projects
FRONTEND=$(just-agent start --workspace-root /projects/frontend --skill code-review)
BACKEND=$(just-agent start --workspace-root /projects/backend --skill security-review)

# Send work to both
just-agent send "$FRONTEND" "Review the latest changes for performance issues" --timeout 120 &
just-agent send "$BACKEND" "Audit dependencies for known vulnerabilities" --timeout 180 &

# Wait for both
wait
```

### Chain agents — feed output into another agent

```bash
# Agent A produces a plan
PLAN=$(just-agent send "$AGENT_A" "Create an implementation plan for feature X")

# Agent B executes the plan in a different project
just-agent send "$AGENT_B" "Execute this plan: $PLAN"
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

| Variable                    | Purpose                                          |
| --------------------------- | ------------------------------------------------ |
| `JUST_AGENT_DAEMON_URL`     | Daemon address (default `http://localhost:3000`) |
| `JUST_LLM_PROVIDER`         | LLM provider (e.g. `deepseek`)                   |
| `JUST_LLM_MODEL`            | Model name (e.g. `deepseek-v4-flash`)            |
| `JUST_LLM_DEEPSEEK_API_KEY` | API key for DeepSeek provider                    |
| `RUST_LOG`                  | Tracing filter (e.g. `just_agent_client=debug`)  |

## Client library

For Rust programs that need more control than the CLI offers, the
`just-agent-client` crate provides the same operations as async methods:

```rust
use just_agent_client::DaemonClient;

let client = DaemonClient::new("http://localhost:3000");

// Spawn, send, kill
let id = client.spawn(Some("/project"), vec!["code-review"], None).await?;
let result = client.send_prompt(&id, "Review src/main.rs", 120).await?;
client.kill_agent(&id).await?;

// Additional client methods:
let usage = client.agent_status(&id).await?;
```
