---
name: Kallip CLI
description: kallip CLI usage — the agent's primary interface for self-management, subagent orchestration, skill discovery, approvals, policy, budget, and coordination
---

# kallip CLI Skill

`kallip` is the headless CLI that agents use to coordinate with the daemon and manage their own runtime. It is the **primary tool** for nearly all agent operations beyond raw shell commands.

## Invocation

```bash
kallip <command>
```

All commands require `KALLIP_AUTH_TOKEN` (env) and optionally `KALLIP_DAEMON_URL` (default `http://127.0.0.1:3000`). These are pre-set in the agent environment.

## Command Reference

### `status` — Agent context usage

```bash
kallip status <ID>
```

Shows context token usage and recent retry history. Use to check your own context pressure or a subagent's before sending more work.

### `activity` — Report current activity (self-only)

```bash
kallip activity "reading docs/x.md"
kallip activity ""    # clear
```

Update your activity label so your supervisor knows what you're doing. Keep it short.

### `message` — Send a message to an agent

```bash
kallip message <ID> <MESSAGE>
```

Fire-and-forget (202 Accepted). The daemon processes asynchronously. Poll `status` or `subagent list` to observe results.

### `subagent` — Manage direct subagents

```bash
kallip subagent spawn --role <ROLE> [--prompt <PROMPT>] [--workspace-root <DIR>] [--permission-class <normal|guest>] [--skill <SKILL>] [--description <DESC>]
kallip subagent list
kallip subagent remove <ID>
kallip subagent interrupt <ID>
kallip subagent metadata <ID> [--role <ROLE>] [--description <DESC>]
```

`--role` is **required** for spawn. It is a short label like `researcher`, `reviewer`. Skills can be activated via repeated `--skill` flags.

Scoping (server-enforced):
- `spawn` / `metadata` — restricted to **direct supervisor** only.
- `remove` / `interrupt` — open to **any ancestor** (superior).

### `approval` — Manage approvals

```bash
kallip approval list [--status <STATUS>] [--all] [--limit <N>] [--offset <N>] [--requested-by <ID>] [--reverse]
kallip approval get <ID>
kallip approval approve <ID>
kallip approval deny <ID> [REASON]
```

Approvals are tool actions that need supervisor sign-off before execution. Statuses: `pending` → `committed` → `approved`/`denied` → `redeemed`/`cancelled`.

Default list shows **committed** (awaiting decision). Use `--all` for every status.

### `policy` — Agent permissions and tool policy

```bash
kallip policy show <ID>          # full permissions + effective tool policy
kallip policy get <ID>           # bare tool-policy map only
kallip policy set <ID> <TOOL> <DECISION>          # allow | ask | deny | classify
kallip policy exec-set <ID> <COMMAND> <DECISION>  # per-command bash_exec override (superior-only)
kallip policy exec-get <ID>      # show bash_exec command overrides
```

`exec-set` controls per-command bash_exec overrides (e.g. `cargo`, `git`, `sudo`). Superior-only.

### `budget` — Daemon-wide token budget

```bash
kallip budget get
kallip budget set <AMOUNT>        # =0 pauses all agents
kallip budget increase <AMOUNT>
kallip budget decrease <AMOUNT>
```

Amounts support K/M/G suffixes (e.g. `100M`, `500K`, `1G`). Budget is daemon-wide, not per-agent.

### `skill` — Skill discovery and promotion

```bash
kallip skill paths                          # show shared + local skill directories
kallip skill meta <NAME>                    # show metadata for a skill
kallip skill promote submit <NAME>          # request promotion of local → shared
kallip skill promote list [--status <STATUS>]
kallip skill promote show <ID>              # diff review of old/new content
kallip skill promote approve <ID>           # approve (root agent)
kallip skill promote deny <ID> [REASON]
```

Skills live as `<name>.md` files with YAML frontmatter (`name`, `description`).

- **Local** dir (writable by the agent): shown by `kallip skill paths`.
- **Shared** dir (operator-managed): write via `promote submit` → root agent review.

### `dirlock` — Directory write-locks (cross-agent mutual exclusion)

```bash
kallip dirlock acquire <PATH> [--timeout-secs <N>]
kallip dirlock release <PATH> [--timeout-secs <N>]
kallip dirlock status                        # dirs this agent currently holds
kallip dirlock who <DIR>                     # who holds the lock, or "unlocked"
```

On `acquire` conflict the daemon returns the holder agent ID — message it to coordinate. `release` is idempotent.

## Common Patterns

For delegation patterns (async notification, parallel work, guest sandboxing,
skill review), see the `agent/subagent-management` skill.

Quick reference:
```bash
# Capture child ID immediately
CHILD=$(kallip subagent spawn --role worker --prompt "do work")

# Async: child messages you when done (preferred over sleep polling)
kallip message "$CHILD" "When done, message agent $KALLIP_ID with results."

# Dirlock for shared directory access
kallip dirlock acquire /path/to/shared
# ... do work ...
kallip dirlock release /path/to/shared
```

## Important Notes

- `subagent` commands use `KALLIP_ID` (current agent) as the supervisor — only meaningful inside an agent context.
- `activity` is self-only; you cannot set another agent's activity.
- `budget` is daemon-wide; `budget set 0` pauses **all** agents.
