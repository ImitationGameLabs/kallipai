# `kallip` Reference

This is the CLI an agent uses to coordinate with other agents and manage its own
subagents and runtime concerns.

All subcommands use `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_TAGMA_URL`
(env, default `http://127.0.0.1:3000`).

## Subcommands

### `message` — Send a message to an agent

```bash
kallip message <ID>
```

Sends a message to the agent's input queue. The message text is read from
the full stdin (multiline — pipe, heredoc, or `< file` all work); there is
no text argument, so shell expansion can never corrupt the message. The
tagma accepts the message immediately (202 Accepted) and processes it
asynchronously. Poll `status` to observe results. On success the CLI
prints a one-line JSON echo of the accepted text (and nothing on failure —
a failed send never looks delivered):

```json
{"kallip.message.sent":{"to":"<id>","text":"<message>","queue_depth":0}}
```

`queue_depth` counts messages queued ahead of this one (0 = immediate
processing); the tagma may also attach a `"warning"` note (e.g. when the
message was buffered for an off-duty agent). The echo key is
distinct from the lesche marker so local clients never render an
agent-to-agent send as a user chat line.

Prefer a quoted heredoc (`<<'EOF'`, delimiter quoted) so the shell performs
no expansion at all: backticks and `$` stay literal, and multiline text
needs no escaping. A pipe works too (`echo 'text' | kallip message <ID>`)
for programmatically produced text. An empty stdin sends an empty message
— the success echo makes that immediately visible.

```bash
# Backticks and $ stay literal inside a quoted heredoc.
$ kallip message "$AGENT_ID" <<'EOF'
Run `cargo test` and report $CARGO_TARGET_DIR.
EOF

# Or pipe it:
$ echo 'List all TODO comments in src/' | kallip message "$AGENT_ID"
```

### `status` — Show agent context usage

```bash
kallip status <ID>
```

Prints context token usage and recent retry history for the agent.

### `subagent` — Manage direct subagents

```bash
kallip subagent <subcommand> [args]
```

Manage the **current agent's direct subagents**. The acting supervisor is taken
from the `KALLIP_ID` env var, so these commands only make sense inside an
agent context — they error if it is unset. `subagent` is the sole management
entry point; spawning, listing, removing, interrupting, and relabeling agents
all go through here.

| Subcommand                | Purpose                                          |
| ------------------------- | ------------------------------------------------ |
| `subagent spawn`          | Spawn a direct subagent (`--role` required).     |
| `subagent list`           | List the current agent's direct subagents.       |
| `subagent remove <ID>`    | Remove a direct subagent.                        |
| `subagent interrupt <ID>` | Interrupt a direct subagent's current operation. |
| `subagent metadata <ID>`  | Update a direct subagent's role/description.     |

Scoping notes (server-enforced):

- `subagent metadata` is restricted to the **direct supervisor**
  (`require_direct_supervisor`); a grandparent cannot relabel a grandchild.
- `subagent remove` / `subagent interrupt` authorize **any ancestor**
  (`require_superior`), so the direct-subagent framing here is a CLI
  convenience, not a server-side restriction.
- `subagent spawn` requires a non-empty `--role`; the tagma rejects subagents
  with an empty role.
- `subagent spawn --permission-class {normal,guest}` explicitly **downgrades**
  the subagent's FS-access class below its tier ceiling (e.g. a `normal` parent
  spawning a read-only `guest` reviewer). The tagma rejects a value above the
  tier ceiling or the parent's own class with `403`. Omit to grant the tier
  ceiling. The granted class is shown by `kallip`/`GET /agents/{id}/permissions`.

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --description "reviews PRs" < /dev/null
b4c2d3e5-...
```
The spawn reads an optional initial prompt from stdin: `< /dev/null` above
means "no prompt" and keeps the spawn from swallowing a surrounding script's
stdin when the id is captured. Pipe or heredoc the prompt instead.

### `approval` — Manage approvals

Subcommands for listing, inspecting, and responding to approvals
(tool actions that require supervisor approval before execution).

#### `approval list` — List approvals

```bash
kallip approval list [--offset <N>] [--limit <N>] [--requested-by <ID>] [--status <STATUS>] [--all] [--reverse]
```

Lists approvals across all agents visible to the authenticated identity.
Default shows committed actions (awaiting approval); use `--all` to see every status or
`--status` to filter by a specific status
(committed, approved, denied, redeemed, cancelled).

```bash
$ kallip approval list --limit 5 --status committed
```

#### `approval get` — Show approval details

```bash
kallip approval get <APPROVAL_ID>
```

Shows full details for a single approval.

```bash
$ kallip approval get "ap_a1b2c3d4..."
```

#### `approval approve` — Approve a committed action

```bash
kallip approval approve <APPROVAL_ID>
```

Approve a committed approval. The agent will be notified and can redeem the action.

```bash
$ kallip approval approve "ap_a1b2c3d4..."
```

#### `approval deny` — Deny a committed action

```bash
kallip approval deny <APPROVAL_ID> [REASON]
```

Deny a committed approval with an optional reason.

```bash
$ kallip approval deny "ap_a1b2c3d4..." "too risky"
```

## Usage patterns

### Delegate work to a subagent

```bash
# Spawn a subordinate, then send it work and poll its progress
CHILD=$(kallip subagent spawn --role researcher <<'EOF'
explore the codebase
EOF
)
kallip message "$CHILD" <<'EOF'
Summarize the project structure
EOF
kallip status "$CHILD"
```

## Multi-agent orchestration

Agents use this CLI to manage their own subagents. A single tagma can host
agents across multiple projects simultaneously.

### Parallel subagents

```bash
# Spawn two subagents for different scopes
FRONTEND=$(kallip subagent spawn --role reviewer --workspace-root /projects/frontend < /dev/null)
BACKEND=$(kallip subagent spawn --role auditor --workspace-root /projects/backend < /dev/null)

# Send work to both
kallip message "$FRONTEND" <<'EOF' &
Review the latest changes for performance issues
EOF
kallip message "$BACKEND" <<'EOF' &
Audit dependencies for known vulnerabilities
EOF

# Wait for both sends to complete
wait
```

### Inspect and control subagents

```bash
# List your direct subagents
kallip subagent list

# Check a subagent's context usage before sending more work
kallip status $CHILD

# Interrupt a running subagent gracefully (without removing it)
kallip subagent interrupt $CHILD
```

## Environment variables

`KALLIP_AUTH_TOKEN` (required) and `KALLIP_TAGMA_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including LLM provider configuration and agent tuning parameters, see [env.md](env.md).

## Client library

For Rust programs that need more control than the CLI offers, the
`kallip-client` crate provides the CLI operations as async methods, plus a
few operator/library-only paths (event streaming, subagent spawn, root lookup):

```rust
use kallip_client::TagmaClient;

let client = TagmaClient::builder("http://127.0.0.1:3000")
    .auth_token(token)
    .build();

// The tagma owns a single root agent (eagerly created at startup); fetch it.
let root = client.get_root_agent().await?;
let id = root.id;

// Send a message (fire-and-forget)
client.post_message(&id, "Review src/main.rs").await?;

// Stream events (CLI exposes status/activity instead), check status.
let mut stream = client.event_stream(&id).await?;
let usage = client.agent_status(&id).await?;
// Note: the root cannot be removed (tagma-managed); `remove_agent` is for
// subagents only.
```

The root agent is tagma-managed: it is created once at startup from env vars
(`KALLIP_WORKSPACE_ROOT`, `KALLIP_MAX_TOOL_ROUNDS`,
`KALLIP_ROOT_AGENT_PERMISSION_CLASS`; see [env.md](env.md)) and surfaced via
`get_root_agent()`. `spawn()` is for **subagents** only — it requires
`created_by`.
