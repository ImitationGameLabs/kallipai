---
name: Planning
description: When a task is too non-trivial to just do — a change, research, an investigation, or a design — and you should plan before acting
---

# Planning — frame, then act

Plan before acting on a non-trivial task, so you understand it and the user
signs off before you spend the work. The standard for what makes a plan good
lives in `what-makes-a-good-plan`; this skill owns the workflow.

## When to use

Load this when the task is not a single obvious step or a direct answer —
multi-step, uncertain, or with real stakes (a code change, a refactor, deep
research, a debug, a design).

## When NOT to use

- For a one-shot answer or a single obvious step, because planning it costs more
  than it saves — just do it.
- To consult what makes a plan good (anatomy, shapes), because this skill only
  runs the workflow — use `what-makes-a-good-plan`.
- To author a skill, because that has its own workflow — use
  `skill/creating-a-skill`.

## The sequence

**Decide to plan.** Is the task non-trivial? If yes, scope it; if not, act
directly, because planning a one-step task wastes the user's time.
Done when:

- you have decided plan-versus-act, and the task is scoped

**Frame the task.** State it precisely and name the "why", separating what is
known from what is not, because a precise frame is what the rest of the plan is
checked against.
Done when:

- there is a precise task statement with the why, and the key unknowns are named

**Gather context.** Read the touch-point files or sources the task turns on,
because the change shape's must-haves (touch-point paths, reuse) cannot be named
without reading — touch points come from actual reading, not inference.
Read text and code through aifed (`outline` for large files, then
`read`) when the task turns on files — the hashlines stay valid if
the plan proceeds to edits.
Done when:

- the relevant files or sources are read, and any touch points come from that reading

**Sketch one approach.** The recommended path and its shape, per
`what-makes-a-good-plan` (change / research / investigation / design). Resolve
genuine forks with the user and decide the rest yourself, because most choices
are yours to make — only genuine forks need the user.
Done when:

- the approach is concrete with its key steps, and no approach-level fork is blocking

**Write the plan.** Save the plan to `/tmp/plans/<short-name>.md`
(kebab-case short name; `mkdir -p /tmp/plans` first), covering all
core items plus this shape's must-haves, per `what-makes-a-good-plan`
— not a partial re-list, because a partial list silently drops criteria
the plan must meet. The fixed location gives reviewers and the
review-change step a known path to reference. These are session
artifacts — /tmp is ephemeral across reboots, so the plan persists
for the session but not beyond.
When a step edits existing text files, name aifed as its tool: load
the `aifed` skill and run its required `aifed --skill` before the
first edit, then do the edits through aifed — writing this into the
plan carries the guarantee to execution time.
The plan file's first line is `Status: draft`, because the marker

lets a reader or concurrent reviewer know the plan is not yet final.
Done when:
- the plan is executable — each step names what to do and how to verify it
- every step that edits existing text files names aifed as the tool
- the plan is at `/tmp/plans/<short-name>.md`
- the plan file starts with `Status: draft`

**Review the plan.** Check it against every core item and this shape's must-haves
in `what-makes-a-good-plan`, because a plan that is wrong wastes the execution.
Then run an independent review (see `agent/subagent-management`),
because a fresh reader catches the author's blind spots — have it
confirm the approach resolves the framed task, not just that the
plan is well-formed. Include the plan's absolute path
(`/tmp/plans/<short-name>.md`) in the reviewer's message, because a reviewer
should not have to guess which plan to read — this is the producer
side of `deep-review`'s pass-the-path instruction. If it requests
changes, fix and re-review per the review-fix loop in
`skill/reviewing-a-skill`.
Done when:

- the plan meets every core item and this shape's must-haves, and an independent reviewer confirms the approach resolves the framed task, with findings resolved or accepted-with-reason (a loop exit — see `skill/reviewing-a-skill`)

**Execute, checkpointing the irreversible.** Get approval before hard-to-reverse
or outward-facing actions, then act; surface at natural boundaries or when the
plan must change. When in doubt, or when an action touches shared state, other
humans, or anything you cannot trivially undo yourself, treat it as irreversible.
Update the plan's status to `in-progress` at the start of execution,
because it signals active implementation so a re-reviewer knows the
context.
Done when:
- the plan's status is `in-progress`
- the task is complete, or the plan is revised with the user
- the plan is archived: status updated to `done`, then `mv` to
  `/tmp/plans/archived/` (`mkdir -p /tmp/plans/archived` first)

## Anti-patterns

- **Acting before framing** — tool calls on a task you have not precisely stated
  waste work on the wrong thing, because the rest of the plan is checked against
  that frame; frame first.
- **No checkpoint before irreversible action** — a hard-to-reverse move without
  approval cannot be undone, because reversibility is the only safety net; get
  approval first.
- **Executing straight after writing** — a plan you did not re-check against the
  standard carries the criteria you silently dropped, because a wrong plan wastes
  the execution; re-check against the standard before acting.
