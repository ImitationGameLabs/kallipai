---
name: Subagent Management
description: How to spawn, coordinate, message, and clean up subagents — including permission classes, dirlock isolation, and common pitfalls
---

# Subagent Management

Subagents are the primary way to parallelize work, delegate tasks, and test
sandboxed environments. This skill covers the full lifecycle: spawn, message,
monitor, and clean up. For command flags and value types, run
`kallip --reference` (the `subagent`, `message`, `status`, and `dirlock`
sections).

## Permission Classes

Every agent has a `PermissionClass` that controls filesystem access:

| Class      | Read        | Write                            | Secrets                                                            | Notes                 |
| ---------- | ----------- | -------------------------------- | ------------------------------------------------------------------ | --------------------- |
| **Normal** | Broad (`/`) | Workspace + dirlocks + `/tmp`    | Readable (no hide-holes)                                           | Default for depth 0–1 |
| **Guest**  | Broad (`/`) | Read-only (`/tmp` baseline only) | Hidden (tmpfs overlay on `~/.ssh`, `~/.gnupg`, `~/.aws`, profiles) | Default for depth 2–3 |

Key rules:

- **Depth-based ceiling**: depth 0/1 → Normal, depth 2/3 → Guest. A subagent's
  class cannot exceed its tier ceiling or its supervisor's class.
- **Explicit override**: `--permission-class guest` lets a Normal supervisor
  spawn a Guest child directly (downgrade only — never upgrade).
- **Both classes** get `readonly_holes` for peer workspaces (other agents'
  locked directories are bind-mounted read-only).
- **Data tree** (`$KALLIP_DATA_DIR/agents/<id>/`) is read-only for both
  classes. Shared skills live in the shared skill directory, writable only by
  the root agent.

## Spawning Subagents

`kallip subagent spawn` prints the new agent ID on stdout — **capture it
immediately** (e.g. `CHILD=$(kallip subagent spawn ...)`) so you can message
and clean up the agent.

### Workspace constraints

- A subagent's `--workspace-root` must be **within the supervisor's workspace**.
- The directory must **exist** before spawn (tagma canonicalizes it).
- The subagent gets an **auto-acquired dirlock** on its workspace (Normal only;
  Guests hold no workspace lock).

### Role is required

`--role` is mandatory for every subagent spawn (the tagma rejects a spawn
without it). Use short descriptive labels: `researcher`, `reviewer`,
`tester`, `worker`.

## Messaging & Monitoring

`kallip message <ID>` sends a task; messages are **asynchronous** — the tagma
queues them and the subagent processes them in order. Poll `kallip status
<ID>` to check whether the agent is `idle` (done), `busy` (still working), or
`faulted` (restore failed — see below). `kallip subagent list` shows your
direct children.

## Dirlock & Workspace Isolation

Each Normal agent holds an exclusive write-lock on its workspace for its
lifetime. This means:

- **Agent A cannot write Agent B's workspace** (bind-mounted read-only).
- **A supervisor cannot write a subagent's workspace** (the subagent holds the lock).
- **Nested delegation is allowed**: a child whose workspace is inside the
  supervisor's workspace acquires its own lock via the delegation chain.

```text
Supervisor workspace: /project
├── supervisor can write /project/*              (holds the lock)
├── child WS:        /project/sub
│   ├── child can write /project/sub/*           (child holds nested lock)
│   └── supervisor CANNOT write /project/sub/*   (readonly hole in supervisor's view)
└── sibling WS:      /project/other
    └── supervisor CANNOT write /project/other/* (sibling child holds the lock)
```

If you need to write to a shared directory, take an explicit dirlock
(`kallip dirlock acquire` / `release`). On conflict, `acquire` returns the
holder's agent ID — message it to coordinate.

## Cleanup

`kallip subagent interrupt <ID>` cancels a busy subagent's current round but
keeps it alive; `kallip subagent remove <ID>` removes it (the agent must be
idle or faulted, with no active subagents of its own). Always clean up test
subagents after use. Removed agents are archived (not deleted), and their
workspace dirlocks are released.

### Faulted agents (restore failure)

If a subagent's workspace is missing when the tagma restarts, the agent is
restored in a `faulted` state — it has no running task but remains in the
registry with its metadata and a `faulted_reason`. Faulted agents appear in
`subagent list`, can be `remove`d (data is archived), but cannot receive
messages or prompts. This is not an error — clean them up with `remove`.

## Decision rules

- If you are at depth 2-3, then the ceiling is Guest regardless (you cannot spawn Normal), because a child's class cannot exceed its tier ceiling.
- If you are at depth 0-1 and the work is untrusted or must not see secrets, then spawn Guest (`--permission-class guest`), because Guests are workspace-read-only with secrets hidden under a tmpfs overlay; otherwise spawn Normal, because it can write within its workspace and hold a dirlock.
- If the subagent is busy and you only want to cancel the current round, then `interrupt`, because it keeps the agent alive for reuse. If it is idle/faulted and you are done, then `remove`, because it archives the agent and releases its workspace dirlock.

*Avoid:* pointing `--workspace-root` at a sibling of your workspace, because the tagma rejects siblings with 403 — it must be a subdirectory within your workspace.

## Common Patterns

### Delegate with async notification (preferred)

Instead of polling with `sleep`, have the subagent message you when done.
The message arrives as a new turn, waking you automatically — no wasted
waits. The subagent learns your ID from `$KALLIP_ID` in your message, or you
pass it explicitly. This is the natural coordination pattern — no sleep or
status-polling needed.

### Guest subagent for untrusted work

Spawn with `--permission-class guest` for untrusted code: the guest is
read-only (workspace RO, no writes) with secrets hidden.

### Independent review (2+1 pattern)

For important work, spawn two Guest reviewers with differentiated prompts
(e.g. clarity vs. robustness) and synthesize their feedback yourself; each
messages you when done, and you resolve disagreements as decision points.

## Pitfalls

- **`kallip` must be in PATH** for subagents to coordinate. If a subagent
  reports `kallip: command not found`, it cannot spawn grandchild agents, use
  dirlock, or send messages.
- **Workspace must exist** before spawn — `mkdir -p` first.
- **Tagma restart releases all dirlocks** — workspaces may become writable
  again until agents are restored.
- **Subagent env** has `KALLIP_ID`, `KALLIP_AUTH_TOKEN`, `KALLIP_TAGMA_URL`,
  `KALLIP_SUPERVISOR_AGENT_ID` (the supervisor), and `KALLIP_ROOT_AGENT_ID`
  (the root) — but NOT `KALLIP_DATA_DIR`. Use the agent's known path
  (`~/.local/share/kallip/agents/<id>/`) instead.
  (`KALLIP_SUPERVISOR_AGENT_ID` is absent, not empty, for the root agent.)
- **`subagent list` only shows direct children** — use the HTTP API
  (`GET /agents?created_by=<id>`) for the same, or check grandchildren via
  their supervisor.
- **Inter-agent messages carry a sender header.** Messages arrive with a
  `[From: agent <id> (role: <role>, <relation>)]` header automatically
  attached, so you always know which agent spoke and the hierarchy relation —
  no need to have subagents role-tag their messages.
- **Long results should go to files.** `kallip message` is fine for short
  results. For long output (reviews, analysis, logs), have the subagent write
  to a file in its workspace and reference the path in the message — the
  supervisor reads it.
