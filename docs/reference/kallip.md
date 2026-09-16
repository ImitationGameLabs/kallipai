---
title: kallip Reference
description: CLI reference for the headless kallip client.
order: 70
---

## kallip Reference

This is the CLI an agent uses to coordinate with other agents and manage its own
subagents and runtime concerns.

All subcommands use `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_TAGMA_URL`
(env, default `http://127.0.0.1:3000`).

### Subcommands

#### `message` — Send a message to an agent

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

#### `status` — Show agent context usage

```bash
kallip status <ID>
```

Prints context token usage and recent retry history for the agent.

#### `subagent` — Manage direct subagents

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
  the subagent's FS-access class below the parent's own class (e.g. a `normal`
  parent spawning a read-only `guest` reviewer). The tagma rejects a value
  above the parent's class with `403`. The flag is required. The granted class
  is shown by `kallip`/`GET /agents/{id}/permissions`.

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --description "reviews PRs" < /dev/null
b4c2d3e5-...
```

The spawn reads an optional initial prompt from stdin: `< /dev/null` above
means "no prompt" and keeps the spawn from swallowing a surrounding script's
stdin when the id is captured. Pipe or heredoc the prompt instead.

#### `profile-set` — Manage named profile sets

Inspects and rewires the named profile sets of `profiles.toml` at runtime
(see [env.md](env.md)):

```bash
$ kallip profile-set list
default set: primary
primary: 2 profile(s), 3 agent(s), modalities: text
cheap: 1 profile(s), 0 agent(s), modalities: text
$ kallip profile-set bind reviewer cheap
Bound reviewer to profile set cheap.
$ kallip profile-set remove cheap --force
Removed profile set cheap (interrupted: reviewer).
```

`list` prints the default marker plus per-set profile and agent counts,
and each set's effective modalities (the intersection across member
declarations).
`bind` takes effect on the agent's next wake-up (parked agents pick the new
binding up at their next restore). `remove` refuses while the set is the
default, while the root agent is bound to it, or while other agents hold
bindings and `--force` is absent; with `--force` every binder is
interrupted (faulted binders skip straight to the dangling state) and the
output names them (by role when the record has one, else by id).

#### `approval` — Manage approvals

Subcommands for listing, inspecting, and responding to approvals
(tool actions that require supervisor approval before execution).

##### `approval list` — List approvals

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

##### `approval get` — Show approval details

```bash
kallip approval get <APPROVAL_ID>
```

Shows full details for a single approval.

```bash
$ kallip approval get "ap_a1b2c3d4..."
```

##### `approval approve` — Approve a committed action

```bash
kallip approval approve <APPROVAL_ID>
```

Approve a committed approval. The agent will be notified and can redeem the action.

```bash
$ kallip approval approve "ap_a1b2c3d4..."
```

##### `approval deny` — Deny a committed action

```bash
kallip approval deny <APPROVAL_ID> [REASON]
```

Deny a committed approval with an optional reason.

```bash
$ kallip approval deny "ap_a1b2c3d4..." "too risky"
```

#### `file` — Content transfer against the files service

Upload, download, deliver, and list records on the files service
(`kallip-files`; HTTP reference in [files-api.md](files-api.md)). The
acting principal is the tagma named by the bearer token (the spawn env);
`--space self` is its own region, `shared` the space's shared region.

```bash
$ kallip file put <PATH> --file <FILE> [--json]   # PATH may be relative for a tagma: it lands in its own region (e.g. images/x.png)
$ kallip file get <ID> [--out <FILE>]
$ kallip file send <ID> (--to-tagma <TAGMA> | --to-user <USER>) [--json]
$ kallip file ls --space self|shared [--prefix <PREFIX>] [--limit <N>] [--json]
```

Credentials ride the environment, never flags: `KALLIP_POLIS_URL`
(the platform edge origin — the CLI derives `<origin>/v1/files` from it,
defaulting to the public deployment) and `KALLIP_FILES_TOKEN` (a tagma's
long-lived bearer). Provision them where the CLI runs; agent shells inherit the
tagma's environment as it stands at spawn time, and the boot sweep
removes `KALLIP_FILES_TOKEN` first, so a provisioned token never
reaches an agent shell
(server-side file fetches authenticate with the tagma's registered
enrollment credential). `--json` prints successful responses as
JSON; `get` buffers the content (capped by the service's max body
size) and writes it to stdout (or `--out`) — content is never
JSON-wrapped.

#### `image` — Read images into the conversation

Ingest an image into this agent's live context (the tagma enforces the
bound set's modalities and records the turn). Three target forms:

- a local path: the bytes land in the tagma's own content-addressed
  attachment store (keyed by their SHA-256 hash) and the turn records
  them; the command prints the blob id and the turn id.
- `--id`: a files record id, fetched by the tagma under its
  own registered credential.
- `--blob`: an already-stored attachment blob, re-ingested
  by content address — no bytes travel.

A parseable UUID without path separators reads as a record id;
`--id` and `--path` pin the interpretation, and `--blob` is never
guessed. The media type comes from `--media-type` or the file
extension (default `image/png`; svg is refused as a non-raster image
unless `--media-type` overrides it). Path-form images must fit the
tagma's request body limit — downsample large pictures first.

```bash
$ kallip image read <PATH-or-ID> [--id] [--path] [--blob] [--media-type <TYPE>] [--caption <TEXT>]
```

### Usage patterns

#### Delegate work to a subagent

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

### Multi-agent orchestration

Agents use this CLI to manage their own subagents. A single tagma can host
agents across multiple projects simultaneously.

#### Parallel subagents

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

#### Inspect and control subagents

```bash
# List your direct subagents
kallip subagent list

# Check a subagent's context usage before sending more work
kallip status $CHILD

# Interrupt a running subagent gracefully (without removing it)
kallip subagent interrupt $CHILD
```

### Environment variables

`KALLIP_AUTH_TOKEN` (required) and `KALLIP_TAGMA_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including LLM provider configuration and agent tuning parameters, see [env.md](env.md).

### Client library

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
