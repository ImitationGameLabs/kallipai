---
name: Kallip CLI
description: When to reach for the kallip CLI and the behaviors clap won't tell you; run `kallip --reference` for full command syntax
---

# kallip CLI

`kallip` is the headless CLI that agents use to coordinate with the tagma and
manage their own runtime — the primary tool for nearly all agent operations
beyond raw shell commands.

## Getting Started

kallip ships its own complete, always up-to-date reference. For any command
syntax (flags, value types, defaults), run:

```bash
kallip --reference
```

This is **progressive disclosure**: this skill tells you _when_ and _why_ to
use kallip; `kallip --reference` gives you the _full syntax_. You don't need to
memorize flags — just know to run it.

For multi-command work, pin the reference so it stays available across turns:

```bash
kallip --reference > /tmp/kallip-ref.md
```

Read it, then in the next turn `context_pin_last` (kind `tool-result`, label
`kallip:reference`). `context_unpin kallip:reference` when the work is done.

## Semantics to remember

These are the behaviors clap can't express — keep them in mind even without the
reference pinned:

- **Messaging is fire-and-forget.** `kallip message` returns immediately (202);
  the tagma processes asynchronously. Poll `kallip status <ID>` or
  `kallip subagent list` to observe results.
- **`activity` and `lesche send` are self-only.** The target is always the
  calling agent (from `KALLIP_ID`); you cannot set another agent's activity.
- **Approvals gate risky tool actions.** State machine: `pending` -> `committed`
  -> `approved`/`denied` -> `redeemed`/`cancelled`. `kallip approval list`
  defaults to **committed** (awaiting your decision); `--all` for every status.
- **Budget is tagma-wide, not per-agent.** `kallip budget set 0` pauses **all**
  agents.
- **Dirlock is cross-agent mutual exclusion.** On `acquire` conflict the tagma
  returns the holder's agent ID — message it to coordinate. `release` is
  idempotent.
- **Subagent scoping (server-enforced):** `spawn` / `metadata` are restricted
  to the **direct supervisor**; `remove` / `interrupt` are open to any
  **ancestor**. `spawn --role` is **required at runtime by the tagma** even
  though clap lists it optional.
- **Skill discovery:** run `kallip skill index <skills-path>` once on the skills
  root and pin the output (label `skill:index`) so you always know what skills
  exist.
- **Shared skills are root-only.** Only the root agent authors shared skill
  files; anyone else proposes them in conversation.

## Delegation

For subagent delegation patterns (async notification, parallel work, guest
sandboxing, skill review), see the `agent/subagent-management` skill.
