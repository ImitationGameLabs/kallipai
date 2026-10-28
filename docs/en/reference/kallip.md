---
title: kallip CLI reference
description: CLI reference for the headless kallip client.
order: 70
---

This is the CLI an agent uses to coordinate with other agents and manage its own
subagents and runtime concerns.

All subcommands use `KALLIPAI_AUTH_TOKEN` (mandatory) and `KALLIPAI_TAGMA_URL`
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
{"kallipai.message.sent":{"to":"<id>","text":"<message>","queue_depth":0}}
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

Without an argument it lists the whole fleet instead: one block per agent,
anomalies first.

The per-agent view is a set of blank-line-separated groups — the clock, the
lifecycle state, context and budget, retry records, upstream facts, the
profile set, and the background tasks the agent's shell is running. The
background-task group is **host-side**: the tagma sends it only to the
operator and to the root agent, and leaves the field out for every other
caller, so a body from before the field existed and a body for an
unentitled caller are identical. Each task block carries its id, state,
command, working directory, elapsed time, and captured output size; a list
at the cap is reported as `20+`, a lower bound rather than a count.

### `subagent`: Manage Direct Subagents

```bash
kallip subagent <subcommand> [args]
```

Manage the **current agent's direct subagents**. The acting supervisor is taken
from the `KALLIPAI_ID` env var, so these commands only make sense inside an
agent context; they error if it is unset. `subagent` is the sole management
entry point; spawning, listing, removing, interrupting, and relabeling agents
all go through here.

| Subcommand                | Purpose                                          |
| ------------------------- | ------------------------------------------------ |
| `subagent spawn`          | Spawn a direct subagent (required flags below).  |
| `subagent list`           | List the current agent's direct subagents.       |
| `subagent remove <ID>`    | Remove a direct subagent.                        |
| `subagent interrupt <ID>` | Abort a direct subagent's running round, or cancel the wait/retry it is parked on. |
| `subagent metadata <ID>`  | Update a direct subagent's role/description.     |
| `subagent workspace relocate <ID> <PATH>` | Move a subagent's workspace to a prepared directory. |
| `subagent workspace mv <ID> <PATH>` | Park the agent, then move the workspace directory itself. |

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
- `subagent spawn --context-keep-turns <TURNS>` and
  `--context-keep-tokens-ratio <SHARE>` set that subagent's own manual
  context-eviction window (`context_evict`): how many newest turns
  survive, and what share of the effective budget they may use. Both are
  optional and default to the tagma's `KALLIPAI_CONTEXT_KEEP_TURNS` and
  `KALLIPAI_CONTEXT_KEEP_TOKENS_RATIO`. `TURNS` may be `0` (summary only);
  `SHARE` must be above 0 and at most 1. The CLI refuses any other share —
  `NaN` and infinities included, since JSON carries no non-finite number —
  before sending, and a direct request carrying one is refused with `400`.
- `subagent workspace` (both verbs) is reserved for the **operator or the
  root agent**; a busy agent, or one that still has live subagents, is
  refused. Details and the cache note are below the examples.

```bash
$ kallip subagent list
researcher  idle  ws=/projects/frontend
$ kallip subagent spawn --role reviewer --workspace-root /projects/reviews --profile-set primary --permission-class guest --description "reviews PRs" < /dev/null
b4c2d3e5-...
```

The spawn reads an optional initial prompt from stdin: `< /dev/null` above
means "no prompt" and keeps the spawn from swallowing a surrounding script's
stdin when the id is captured. Pipe or heredoc the prompt instead.

#### `subagent workspace`: Move a Subagent's Workspace

`kallip subagent workspace relocate <ID> <PATH>` moves a subagent to a new
workspace in one command: deactivate, retarget the recorded workspace root,
restore. The target directory must already hold the agent's files (an empty
target is refused); this verb never moves files itself.

`kallip subagent workspace mv <ID> <PATH>` parks the agent, then
moves the recorded workspace directory to the new path (a plain
rename: the source disappears; cross-device moves are refused),
then retargets and restores.

Both verbs are idempotent. Each step probes the current state, so a retried
command resumes from wherever the previous run stopped. A refusal at the park
step (a busy agent, or one with live subagents) leaves the agent
untouched; a failure past it leaves the agent deactivated (cold), never
half-restored. The restore rebuilds the agent's system prompt, so its prefix
cache does not carry over; that is expected after a workspace switch, not a
fault.

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
When a gateway serves the tagma, `list` prints `collection: owner/name`
after the default marker and before the set lines: those set names resolve
inside that collection.
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
(`kallipai-files`; HTTP reference in `docs/en/reference/files-api.md`). The
acting principal is the tagma named by the bearer token (the spawn env);
`--space self` is its own region, `shared` the space's shared region.

```bash
$ kallip file put <PATH> --file <FILE> [--json]   # PATH may be relative for a tagma: it lands in its own region (e.g. images/x.png)
$ kallip file get <ID> [--out <FILE>]
$ kallip file send <ID> (--to-tagma <TAGMA> | --to-user <USER>) [--json]
$ kallip file ls --space self|shared [--prefix <PREFIX>] [--limit <N>] [--json]
```

Credentials ride the environment, never flags: `KALLIPAI_POLIS_URL`
(the platform edge origin: the CLI derives `<origin>/v1/files` from it;
required, and unset is an error: credentials are never sent to an
assumed deployment) and `KALLIPAI_FILES_TOKEN` (a tagma's
long-lived bearer). Provision them where the CLI runs; agent shells inherit the
tagma's environment as it stands at spawn time, and the boot sweep
removes `KALLIPAI_FILES_TOKEN` first, so a provisioned token never
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
- `--id`: a files record id, fetched once under the serving
  platform face's own credential and stored locally as the master
  copy (the command prints the blob id, like the path form).
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

`--id` addresses another agent's inbox; it defaults to `KALLIPAI_ID`.
List output is newest first.

### `lesche`: Deliver Messages through the Chat Relay

`send` targets the user's bilateral conversation by default, a joined
room with `--room`, or a peer tagma's direct session with `--tagma`:

```bash
kallip lesche send [--room <ROOM>] [--tagma <TAGMA>] [--quote <SEQ>] [--mention <PARTICIPANT-ID>]...
kallip lesche rooms
kallip lesche read (--room <ROOM> | --tagma <TAGMA> | --session <SESSION> | --quote <ID>) [--after <N>] [--before <N>] [--limit <N>]
kallip lesche sessions
```

`rooms` lists the rooms this tagma has joined; `read` replays a
conversation's history -- forward from a sequence position (`--after`),
backward from one (`--before`, rooms only), or the single row a reply names
(`--quote`). A quote rides a room read, a direct-session read addressed by
the session id (`--session`, the value an inbound header spells as `direct
<session>`), or no target at all: a bare `--quote <ID>` reads one row of the
bilateral 1:1. `send --quote <SEQ>` points the message at that row (room and
direct sends). `sessions` lists every addressable surface (user 1:1, joined
rooms, direct sessions) in one view. Attachments must live in a workspace the
peer can read.

A delivered message that answers another one names its reference under the
`[From: ...]` header: who wrote that row and a one-line summary of it (at most
256 grapheme clusters of body, with one `…` added when it is cut). The
`kallip lesche read` command that prints the whole row follows whenever the
first line is not the whole row -- the summary was cut, the read produced no
row, or it holds no text. The message itself is always delivered.

When the read produces no row, the first line says which of the two things
happened, and the command that prints the whole row follows either way.
`the quoted row is not in that log` is a fact about the history: the page that
came back did not hold that row, the position leaves no page to fetch, or the
1:1 history store holds no such row. `the quoted row could not be read back` is
a fact about the read: the fetch failed, the payload did not decode, the budget
ran out, or the 1:1 seam has no history store to ask. A failed fetch covers a
surface that answers 404, which it returns for an unknown room or session and
for a reader who is not a member alike, so that answer says nothing about
whether the row is there: the row may be, and its body still never arrived.

The reference goes only to a reader that can read the row it names: today the
root agent on the 1:1 (that transcript is root's own, and the command answers
403 for anyone else) and the receiving agent on a relay surface. The
`| quote=<SEQ>` segment of the header goes only where the reference does. A row
that holds no text renders as `replying to <author>: (no text on that row)` and
still carries the command: what a row holds besides its text is not visible to
the seam that renders the reference.
`send --mention <ID>` (repeatable) names members of that room: the id alone
crosses the wire (the roster resolves the label at render time), the flag is
a room-send one, and a name the room does not hold is refused, not dropped.
Membership is the tagma's judgement, made on the agent's send path against
the room's roster; the CLI itself only refuses the flag away from a room
send, and the browser composer checks the shape and the cap alone -- the
lesche relays the ids on that path as given.
The judgement has two limits, by design. The roster it judges from is the one
the poll last fetched, so a membership change takes up to a poll period (about
a minute) to show: a name the room has just dropped can still pass until then.
And when the rooms listing cannot answer for the room -- the fetch fails, or
its answer does not cover that room -- a send's mentions go out unjudged:
the envelope POST backs off and retries on 503, the rooms GET has no such
retry.
The user's client renders messages as Markdown.

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

Queue, state machine, confirmation cycle, event trail, hard gates, and
closed-task archives, served by the tagma task API:

| Subcommand     | Purpose                                                                                                                                                                                                         |
| ---            | ---                                                                                                                                                                                                             |
| `task create`  | Register a task (`--title ...`); `--require` fixes the confirmer roster (repeatable), resolved to identities at create; `--assignee` defaults to whoever picks it up; plus the association keys                 |
| `task start`   | Pick a queued task up by id; the serial gate applies unless `--force`                                                                                                                                           |
| `task confirm` | File your confirmation toward the close gate, with an optional `--note` and a report body read from `--file`                                                                                                    |
| `task review`  | Move an in-progress task to review                                                                                                                                                                              |
| `task pause`   | Park an in-progress task; no gate applies                                                                                                                                                                       |
| `task resume`  | Unpause a paused task; the serial gate applies unless `--force`                                                                                                                                                 |
| `task note`    | Append a work note to the trail without moving the state machine                                                                                                                                                |
| `task close`   | Close a task with a `--reason` and an optional `--summary`; every registered confirmer must have confirmed unless `--force`. With `--dossier <dir>` the directory packs canonically and archives with the close |
| `task reopen`  | Reopen a closed task; the serial gate applies unless `--force`                                                                                                                                                  |
| `task archive` | Archive a closed task so it leaves the default list view; the closed-only gate applies unless `--force`                                                                                                         |
| `task list`    | List tasks (`--status`, `--assignee`, `--archived`, time-window filters, paging, `--relative-time`)                                                                                                             |
| `task show`    | Show one task's state, association keys, event trail, and token usage over its active windows                                                                                                                   |
| `task export`  | Export one task or all tasks; `--json` emits the machine face                                                                                                                                                   |
| `task extract` | Extract a closed task's content-addressed dossier archive                                                                                                                                                       |
| `task report`  | Read confirmations back from the trail: `report list` (optional `--confirmer` filter, `--out`), `report show` one confirmation (`--confirmer`, `--version`, `--out`)                                            |

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

### `turns`: Read an Agent's Turn History

```bash
kallip turns list <AGENT> [--day <YYYY-MM-DD>] [--before <CURSOR>] [--limit <N>]
    [--fields <NAMES>] [--tools] [--json]
kallip turns show <AGENT> <TURN> [--call <N> [--body [--offset <N>] [--limit <N>]]]
    [--json]
kallip turns export <AGENT> [--day <YYYY-MM-DD>] [--limit <N>] [--pages <N>] [--out <FILE>]
```

The tagma computes every answer from the log it already keeps, so the CLI
never opens a history file: `list` returns one page of turns newest first,
`show` returns one turn with its messages and call rows, and `--call N --body`
returns a byte range of one call's text.

| Subcommand     | Purpose |
| ---            | --- |
| `turns list`   | One page of turns, newest first; `--fields` picks the columns, `--tools` expands each turn's calls under its row, outcome and all, `--json` prints the page as the tagma returned it |
| `turns show`   | One turn: its messages and call rows; `--call N` narrows to one call, `--call N --body` fetches that call's text, and `--json` prints the tagma's document for the turn, the call, or the text range |
| `turns export` | Walks up to `--pages` pages (20 by default) and writes one JSON document: a manifest plus the pages as returned |

A cursor is opaque: copy `next_cursor` into `--before` byte for byte, since
the ordering key belongs to the tagma. `turns export` stays bounded, and its
manifest says which bound stopped the walk (`stopped_by`: `end` when the log
ran out, `pages` when the page bound did) and where a stopped walk resumes
(`next_cursor` and `limit`). Tool output never rides a page: `--call N --body`
reads a range of the text, and every range notice goes to stderr so the bytes
can be piped straight on.

The table and text faces refuse a page whose `schema_version` this build does
not read, so a field that changed meaning cannot print as a dash; `--json` hands
the tagma's document over byte for byte, unknown fields and version included,
and `turns export` keeps each page body the same way. A walk that meets a
second `schema_version` stops instead: one document never mixes two views.
The `profile` column shows provider and model only; `turns show` prints the
profile set, profile id, and effort as well. The `usage` column prints
`prompt→completion`, then `cache:<read>/<write>` when either side is non-zero
(a zero side stays in the pair) and `reason:<tokens>` when it is not zero;
`turns show` prints all five counters on one line, each under its name, a zero
included.

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

`KALLIPAI_AUTH_TOKEN` (required) and `KALLIPAI_TAGMA_URL` (default `http://127.0.0.1:3000`) are the primary variables. For the complete reference including agent tuning parameters, see [Configuration](../configuration/index.md).

### Client Library

For Rust programs that need more control than the CLI offers, a
client library provides the CLI operations as async methods, plus a
few advanced paths (event streaming, subagent spawn, root lookup):

```rust
use kallipai_client::TagmaClient;

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
(`KALLIPAI_WORKSPACE_ROOT`, `KALLIPAI_MAX_TOOL_ROUNDS`,
`KALLIPAI_ROOT_AGENT_PERMISSION_CLASS`; see [Agent core and shell](../configuration/tagma/agent.md)) and surfaced via
`get_root_agent()`. `spawn()` is for **subagents** only; it requires
`created_by`.
