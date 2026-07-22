---
name: Subagent Management
description: How to spawn, coordinate, message, and clean up subagents — including permission classes, dirlock isolation, and common pitfalls
---

# Subagent Management

Subagents are the primary way to parallelize work, delegate tasks, and test
sandboxed environments. This skill covers the full lifecycle: spawn, message,
monitor, and clean up.

## Permission Classes

Every agent has a `PermissionClass` that controls filesystem access:

| Class      | Read        | Write                                        | Secrets                                                            | Notes                 |
| ---------- | ----------- | -------------------------------------------- | ------------------------------------------------------------------ | --------------------- |
| **Normal** | Broad (`/`) | Workspace + dirlocks + skills carve + `/tmp` | Readable (no hide-holes)                                           | Default for depth 0–1 |
| **Guest**  | Broad (`/`) | **Skills carve only**                        | Hidden (tmpfs overlay on `~/.ssh`, `~/.gnupg`, `~/.aws`, profiles) | Default for depth 2–3 |

Key rules:

- **Depth-based ceiling**: depth 0/1 → Normal, depth 2/3 → Guest. A subagent's
  class cannot exceed its tier ceiling or its supervisor's class.
- **Explicit override**: `--permission-class guest` lets a Normal supervisor
  spawn a Guest child directly (downgrade only — never upgrade).
- **Both classes** get `readonly_holes` for peer workspaces (other agents'
  locked directories are bind-mounted read-only).
- **Data tree** (`$KALLIP_DATA_DIR/agents/<id>/`) is read-only except the
  `skills/` subdirectory (the "skills carve") for both classes.

## Spawning Subagents

```bash
# Basic spawn (inherits supervisor's permission class ceiling)
kallip subagent spawn --role <ROLE> [--prompt <PROMPT>] [--workspace-root <DIR>]

# Explicit permission class
kallip subagent spawn --role <ROLE> --permission-class guest

# With skills
kallip subagent spawn --role <ROLE> --skill kallip --skill aifed

# Full example
kallip subagent spawn \
  --workspace-root /path/to/workspace \
  --role researcher \
  --description "Explores the codebase" \
  --permission-class normal \
  --prompt "You are a code researcher." \
  --skill kallip
```

The command prints the new agent ID on stdout. **Capture it immediately**:

```bash
CHILD=$(kallip subagent spawn --role worker --prompt "do work")
```

### Workspace constraints

- A subagent's `--workspace-root` must be **within the supervisor's workspace**.
- The directory must **exist** before spawn (tagma canonicalizes it).
- The subagent gets an **auto-acquired dirlock** on its workspace (Normal only;
  Guests hold no workspace lock).

### Role is required

`--role` is mandatory for all subagent spawns. Use short descriptive labels:
`researcher`, `reviewer`, `tester`, `worker`.

## Messaging & Monitoring

```bash
# Send a task (fire-and-forget, 202 Accepted)
kallip message <ID> "Run the tests and report results"

# Check progress
kallip status <ID>          # context usage + state (idle/busy/faulted)

# List direct subagents
kallip subagent list
```

Messages are **asynchronous** — the tagma queues them and the subagent
processes them in order. Poll `status` to check if the agent is `idle`
(done), `busy` (still working), or `faulted` (restore failed — see below).

## Dirlock & Workspace Isolation

Each Normal agent holds an exclusive write-lock on its workspace for its
lifetime. This means:

- **Agent A cannot write Agent B's workspace** (bind-mounted read-only).
- **A supervisor cannot write a subagent's workspace** (the subagent holds the lock).
- **Nested delegation is allowed**: a child whose workspace is inside the
  parent's workspace acquires its own lock via the delegation chain.

```
Parent workspace:    /project
├── parent can write /project/*          (holds the lock)
├── child WS:        /project/sub
│   ├── child can write /project/sub/*   (child holds nested lock)
│   └── parent CANNOT write /project/sub/*  (readonly hole in parent's view)
└── sibling WS:      /project/other
    └── parent CANNOT write /project/other/* (sibling child holds the lock)
```

If you need to write to a shared directory, use explicit dirlock:

```bash
kallip dirlock acquire /path/to/shared
# ... do work ...
kallip dirlock release /path/to/shared
```

On conflict, `acquire` returns the holder's agent ID — message it to coordinate.

## Cleanup

```bash
# Interrupt a busy subagent (cancels current round, keeps the agent alive)
kallip subagent interrupt <ID>

# Remove a subagent (must be idle or faulted, no active subagents of its own)
kallip subagent remove <ID>
```

Always clean up test subagents after use. Removed agents are archived (not
deleted), and their workspace dirlocks are released.

### Faulted agents (restore failure)

If a subagent's workspace is missing when the tagma restarts, the agent is
restored in a `faulted` state — it has no running task but remains in the
registry with its metadata and a `faulted_reason`. Faulted agents appear in
`subagent list`, can be `remove`d (data is archived), but cannot receive
messages or prompts. This is not an error — clean them up with `remove`.

## Common Patterns

### Delegate with async notification (preferred)

Instead of polling with `sleep`, have the subagent message you when done.
The message arrives as a new turn, waking you automatically — no wasted waits.

```bash
CHILD=$(kallip subagent spawn --role worker --prompt "help with testing")
kallip message "$CHILD" "Run: echo hello > test.txt && cat test.txt.
When done, send a message to agent $KALLIP_ID with your results."
# ... do other work, or go idle and get woken by the notification ...
# The subagent runs: kallip message $KALLIP_ID "DONE: ..."
```

The subagent learns your ID from `$KALLIP_ID` in your message, or you
pass it explicitly. This is the natural coordination pattern — no sleep
or status-polling needed.

### Guest subagent for untrusted work

```bash
kallip subagent spawn --role sandbox --permission-class guest \
  --workspace-root $WS/sandbox --prompt "Run untrusted code safely"
# Guest: workspace RO, secrets hidden, skills carve writable only
```

### Independent review (2+1 pattern)

For important work, spawn two Guest reviewers with differentiated prompts,
synthesize their feedback yourself:

```bash
A=$(kallip subagent spawn --role reviewer-clarity --permission-class guest \
  --prompt "Review from a fresh reader's clarity perspective")
B=$(kallip subagent spawn --role reviewer-robustness --permission-class guest \
  --prompt "Review for robustness, staleness, and failure modes")
# Each messages you when done; synthesize disagreements as decision points
```

## Pitfalls

- **`kallip` must be in PATH** for subagents to coordinate. If a subagent
  reports `kallip: command not found`, it cannot spawn grandchild agents, use
  dirlock, or send messages.
- **Workspace must be a subdirectory** of the supervisor's workspace — siblings
  are rejected with 403.
- **Workspace must exist** before spawn — `mkdir -p` first.
- **Tagma restart releases all dirlocks** — workspaces may become writable
  again until agents are restored.
- **Subagent env** has `KALLIP_ID`, `KALLIP_AUTH_TOKEN`, `KALLIP_TAGMA_URL`
  but NOT `KALLIP_DATA_DIR` — use the agent's known path
  (`~/.local/share/kallip/agents/<id>/`) instead.
- **`subagent list` only shows direct children** — use the HTTP API
  (`GET /agents?created_by=<id>`) for the same, or check grandchildren via
  their parent.
- **Messages carry no sender ID.** Subagent messages arrive as bare text with no indication of which agent sent them. Have subagents prefix their messages with their role or a tag (e.g. `CLARITY_REVIEW: ...`).
- **Long results should go to files.** `kallip message` is fine for short results. For long output (reviews, analysis, logs), have the subagent write to a file in its workspace and reference the path in the message — the supervisor reads it.
