---
title: kallip CLI reference
description: CLI reference for the headless kallip client.
order: 70
---

This is the CLI an agent uses to coordinate with other agents and manage its own
subagents and runtime concerns.

All subcommands use `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_TAGMA_URL`
(env, default `http://127.0.0.1:3000`).

## Subcommands

### `message`: Send a Message to an Agent

```bash
kallip message <ID>
```

Sends a message to the agent's input queue. The message text is read from
the full stdin (multiline: pipe, heredoc, or `< file` all work); there is
no text argument, so shell expansion can never corrupt the message. The
tagma accepts the message immediately (202 Accepted) and processes it
asynchronously. Poll `status` to observe results. On success the CLI
prints a one-line JSON echo of the accepted text (and nothing on failure,
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
, and the success echo makes that immediately visible.

```bash
# Backticks and $ stay literal inside a quoted heredoc.
$ kallip message "$AGENT_ID" <<'EOF'
Run `cargo test` and report $CARGO_TARGET_DIR.
EOF

# Or pipe it:
$ echo 'List all TODO comments in src/' | kallip message "$AGENT_ID"
```

### `status`: Show Agent Context Usage

```bash
kallip status <ID>
```

Prints context token usage and recent retry history for the agent.

### `subagent`: Manage Direct Subagents

```bash
kallip subagent <subcommand> [args]
```

Manage the **current agent's direct subagents**. The acting supervisor is taken
from the `KALLIP_ID` env var, so these commands only make sense inside an
agent context; they error if it is unset. `subagent` is the sole management
entry point; spawning, listing, removing, interrupting, and relabeling agents
all go through here.

| Subcommand                | Purpose                                          |
| ------------------------- | ------------------------------------------------ |
| `subagent spawn`          | Spawn a direct subagent (required flags below).  |
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
- `subagent spawn` requires `--workspace-root`, `--profile-set`, and
  `--permission-class`; unknown profile-set names are rejected at spawn
  time. A non-empty `--role` is required for every subordinate spawn.
- `subagent spawn --permission-class {normal,guest}` explicitly **downgrades**
  the subagent's FS-access class below the parent's own class (e.g. a `normal`
  parent spawning a read-only `guest` reviewer). The tagma rejects a value
  above the parent's class with `403`. The flag is required. The granted class
  is shown by `kallip`/`GET /agents/{id}/permissions`.

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --workspace-root /projects/reviews --profile-set primary --permission-class guest --description "reviews PRs" < /dev/null
b4c2d3e5-...
```

The spawn reads an optional initial prompt from stdin: `< /dev/null` above
means "no prompt" and keeps the spawn from swallowing a surrounding script's
stdin when the id is captured. Pipe or heredoc the prompt instead.

### `profile-set`: Manage Named Profile Sets

Inspects and rewires the named profile sets of `profiles.toml` at runtime
(see [Model configuration methods](../configuration/tagma/methods.md)):

```bash
$ kallip profile-set list
default set: primary
primary: 2 profile(s), 3 agent(s), modalities: text
cheap: 1 profile(s), 0 agent(s), modalities: text
$ kallip profile-set bind reviewer cheap
Bound reviewer to profile set cheap.
```

`list` prints the default marker plus per-set profile and agent counts,
and each set's effective modalities (the intersection across member
declarations).
`bind` takes effect on the agent's next wake-up (parked agents pick the new
binding up at their next restore).

### `approval`: Manage Approvals

Subcommands for listing, inspecting, and responding to approvals
(tool actions that require supervisor approval before execution).

#### `approval list`: List Approvals

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

#### `approval get`: Show Approval Details

```bash
kallip approval get <APPROVAL_ID>
```

Shows full details for a single approval.

```bash
$ kallip approval get "ap_a1b2c3d4..."
```

#### `approval approve`: Approve a Committed Action

```bash
kallip approval approve <APPROVAL_ID>
```

Approve a committed approval. The agent will be notified and can redeem the action.

```bash
$ kallip approval approve "ap_a1b2c3d4..."
```

#### `approval deny`: Deny a Committed Action

```bash
kallip approval deny <APPROVAL_ID> [REASON]
```

Deny a committed approval with an optional reason.

```bash
$ kallip approval deny "ap_a1b2c3d4..." "too risky"
```

### `file`: Content Transfer against the Files Service

Upload, download, deliver, and list records on the files service
(`kallip-files`; HTTP reference in `docs/en/reference/files-api.md`). The
acting principal is the tagma named by the bearer token (the spawn env);
`--space self` is its own region, `shared` the space's shared region.

```bash
$ kallip file put <PATH> --file <FILE> [--json]   # PATH may be relative for a tagma: it lands in its own region (e.g. images/x.png)
$ kallip file get <ID> [--out <FILE>]
$ kallip file send <ID> (--to-tagma <TAGMA> | --to-user <USER>) [--json]
$ kallip file ls --space self|shared [--prefix <PREFIX>] [--limit <N>] [--json]
```

Credentials ride the environment, never flags: `KALLIP_POLIS_URL`
(the platform edge origin: the CLI derives `<origin>/v1/files` from it;
required, and unset is an error: credentials are never sent to an
assumed deployment) and `KALLIP_FILES_TOKEN` (a tagma's
long-lived bearer). Provision them where the CLI runs; agent shells inherit the
tagma's environment as it stands at spawn time, and the boot sweep
removes `KALLIP_FILES_TOKEN` first, so a provisioned token never
reaches an agent shell
(server-side file fetches authenticate with the tagma's registered
enrollment credential). `--json` prints successful responses as
JSON; `get` buffers the content (capped by the service's max body
size) and writes it to stdout (or `--out`); content is never
JSON-wrapped.

### `image`: Read Images into the Conversation

Ingest an image into this agent's live context (the tagma enforces the
bound set's modalities and records the turn). Three target forms:

- a local path: the bytes land in the tagma's own content-addressed
  attachment store (keyed by content hash) and the turn records
  them; the command prints the blob id and the turn id.
- `--id`: a files record id, fetched by the tagma under its
  own registered credential.
- `--blob`: an already-stored attachment blob, re-ingested
  by content address; no bytes travel.

A parseable UUID without path separators reads as a record id;
`--id` and `--path` pin the interpretation, and `--blob` is never
guessed. The media type comes from `--media-type` or the file
extension (default `image/png`; svg is refused as a non-raster image
unless `--media-type` overrides it). Path-form images must fit the
tagma's request body limit; downsample large pictures first.

```bash
$ kallip image read <PATH-or-ID> [--id] [--path] [--blob] [--media-type <TYPE>] [--caption <TEXT>]
```

### `activity`: Report This Agent's Current Activity

```bash
kallip activity <ACTIVITY>   # pass an empty string to clear
```

A short phrase describing what this agent is doing right now
(e.g. "reading docs/x.md"). Self-only.

### `agent`: Read-Only Team Directory

```bash
kallip agent list
```

Lists every agent on this tagma with role, state, since, id, and
workspace. Discovery only; management actions live under `subagent`.

### `budget`: Manage the Tagma-Wide Token Budget

One budget is shared by all agents on the tagma:

```bash
kallip budget get                   # tagma-wide budget status
kallip budget increase <AMOUNT>     # K, M, G suffixes, e.g. 100M
kallip budget decrease <AMOUNT>
kallip budget set <AMOUNT>          # 0 pauses every agent
kallip budget unlimited             # enforcement off, consumption still tracked
```

### `inbox`: Manage This Agent's Message Inbox

```bash
kallip inbox list [--status unread|read|done] [--limit <N>] [--relative-time]
kallip inbox read <MSG_ID>        # marks the message as read
kallip inbox summary              # total and unread counts
kallip inbox done <MSG_ID>
kallip inbox clear [--all]        # done-only unless --all
```

`--id` addresses another agent's inbox; it defaults to `KALLIP_ID`.
List output is newest first.

### `lesche`: Deliver Messages through the Chat Relay

`send` targets the user's bilateral conversation by default, a joined
room with `--room`, or a peer tagma's direct session with `--tagma`:

```bash
kallip lesche send [--room <ROOM>] [--tagma <TAGMA>]
kallip lesche rooms
kallip lesche read (--room <ROOM> | --tagma <TAGMA>) [--after-seq <N>] [--limit <N>]
kallip lesche sessions
```

`rooms` lists the rooms this tagma has joined; `read` replays a
conversation's history from a sequence position; `sessions` lists every
addressable surface (user 1:1, joined rooms, direct sessions) in one
view. Attachments must live in a workspace the peer can read.

### `policy`: Inspect Agent Permissions

```bash
kallip policy show <ID>
kallip policy exec-get <ID>
kallip policy exec-set <ID> <COMMAND> <allow|ask|deny> [--reason <REASON>]
```

`show` prints an agent's full permissions and the active classify
preset. `exec-get` and `exec-set` read and write the per-command
`bash_exec` overrides; `exec-set` is superior-only, and `--reason` is
surfaced to the agent when the decision narrows to ask or deny.

### `skill`: Discover and Inspect Skills

```bash
kallip skill index <PATH> [--depth <N>]
kallip skill meta <PATH>
```

`index` renders a skill directory's index from each file's frontmatter
(`--depth 1` for a flat view, higher for a small subtree); `meta` shows
one skill's metadata, read directly from the filesystem.

### `task`: Task Ledger on the Tagma

Queue, state machine, event trail, hard gates, and closed-task
archives, served by the tagma task API:

| Subcommand | Purpose |
| --- | --- |
| `task start` | Register a task (`--title ...`) or pick a queued one up by id; the serial gate applies unless `--force` |
| `task checkpoint` | Record a work note, a review receipt (`--receipt`), a move to review (`--review`), and/or the waiting marker |
| `task close` | Close a task; every dispatched seat must have filed a receipt unless `--force` |
| `task reopen` | Reopen a closed task |
| `task annotate` | Append a note to the trail without moving the state machine |
| `task dispatch` | Register the seat roster for the current review cycle |
| `task gate-report` | Record the announcement that precedes every chain operation |
| `task chain-op` | Record a chain operation (commit/amend/rebase/reset); requires a newer gate report |
| `task archive` | Archive a closed task so it leaves the default list view |
| `task list` | List tasks (`--status`, `--assignee`, `--archived`) |
| `task show` | Show one task's state, association keys, and event trail |
| `task export` | Export one task or all tasks; `--json` emits the machine face |
| `task extract` | Extract a closed task's content-addressed dossier archive |

### `team`: Declarative Team Management

```bash
kallip team status [--file <FILE>] [--json]
kallip team converge [--dry-run] [--drain] [--force] [--json]
kallip team lock-rebuild [--dir <DIR>]
```

`status` shows the three-way comparison (declaration vs lock vs live
registry), one row per role, with converge's verdict. `converge` plans,
preflights, and executes convergence; it refuses the whole batch on
structural problems, and writes the lock on applied and aborted
outcomes. `lock-rebuild` rebuilds the lock archive from reality,
listing every recovered parked agent.

### Usage Patterns

#### Delegate Work to a Subagent

```bash
# Spawn a subordinate, then send it work and poll its progress
CHILD=$(kallip subagent spawn --role researcher --workspace-root /projects/research --profile-set primary --permission-class normal <<'EOF'
explore the codebase
EOF
)
kallip message "$CHILD" <<'EOF'
Summarize the project structure
EOF
kallip status "$CHILD"
```

### Multi-Agent Orchestration

Agents use this CLI to manage their own subagents. A single tagma can host
agents across multiple projects simultaneously.

#### Parallel Subagents

```bash
# Spawn two subagents for different scopes
FRONTEND=$(kallip subagent spawn --role reviewer --workspace-root /projects/frontend --profile-set primary --permission-class guest < /dev/null)
BACKEND=$(kallip subagent spawn --role auditor --workspace-root /projects/backend --profile-set primary --permission-class normal < /dev/null)

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

#### Inspect and Control Subagents

```bash
# List your direct subagents
kallip subagent list

# Check a subagent's context usage before sending more work
kallip status $CHILD

# Interrupt a running subagent gracefully (without removing it)
kallip subagent interrupt $CHILD
```

### Environment Variables

`KALLIP_AUTH_TOKEN` (required) and `KALLIP_TAGMA_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including LLM provider configuration and agent tuning parameters, see [Configuration](../configuration/index.md).

### Client Library

For Rust programs that need more control than the CLI offers, a
client library provides the CLI operations as async methods, plus a
few advanced paths (event streaming, subagent spawn, root lookup):

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
`KALLIP_ROOT_AGENT_PERMISSION_CLASS`; see [Agent core and shell](../configuration/tagma/agent.md)) and surfaced via
`get_root_agent()`. `spawn()` is for **subagents** only; it requires
`created_by`.
