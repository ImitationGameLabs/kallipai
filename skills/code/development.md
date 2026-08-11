---
name: Development
description: When you are implementing a code or config change end-to-end — from exploration and planning through implementation, draft commit, deep review, and amend
---

# Development — the end-to-end change workflow

This is the ordered workflow for implementing a non-trivial code or config
change from start to finish. Each step delegates the craft to a specialized
skill; this skill owns only the order and the two review checkpoints.

For multi-agent teams, `team/software-development` specializes this
workflow into roles (surgeon, scout, reviewer) and a parallel pipeline;
this skill is the single-agent per-task workflow those roles build on.

## When to use

- You need to implement a change — a feature, a fix — and want
  the full workflow from understanding the codebase through shipping

## When NOT to use

- For a one-line fix or trivial typo — act directly, because the full
  workflow costs more than it saves
- To explore a codebase without changing it — use `code/exploring`
- To write a plan without implementing — use `planning`
- For a bug fix where the root cause is unknown — start with
  `code/debugging` for the investigation workflow, because development
  assumes you know what to change; return here once the cause is found
  and the fix is non-trivial
- For pure structural cleanup that preserves behavior — use `code/refactoring`, because the safety-net discipline (tests verified before and after each small change) is specific to behavior-preserving work

## The sequence

**Explore.** Understand the codebase before touching it. Load `code/exploring`
for the reader's survey, or `code/onboarding` if you need development-depth
conventions (AGENTS.md, dev docs, task-area drill-in). The goal is to name
the touch-point files and reusable patterns before planning.
Done when:

- the touch-point files are identified (with paths)
- reusable functions, patterns, or conventions are noted

**Plan.** Frame the change and sketch one recommended approach. Load
`planning` for the workflow and `what-makes-a-good-plan` for the standard.
For non-trivial designs, load `code/design-thinking` to explore and
evaluate approaches before converging.
The plan must name the touch-point files, the change shape, and a
verification step.
Done when:

- the plan is written with framing, touch points, approach, and verification
- approach-level forks are resolved with the operator
- the plan is pinned in context so it survives context compaction

**Review the plan.** Run `deep-review` on the plan: spawn 2-3 independent
reviewers, collect findings, meta-review, and produce verified findings.
Fix the plan per the findings, re-review if needed (the review-fix loop
lives in `skill/reviewing-a-skill`).
Done when:

- deep-review has ended on the plan (no blocking findings, or accepted-with-reason)

**Implement.** Make the change following the approved plan. Use `aifed` for
all edits. Follow `code/code-comments` for comment quality (why over
what, doc vs internal, marker discipline). Work in small steps,
checking each against the plan. Load `code/testing` for test
quality if the change includes tests. If the implementation reveals
the plan was wrong, stop and revise the plan.
Done when:

- every plan item is implemented
- the change compiles and passes the plan's verification step

**Draft commit.** Load `code/committing` for the commit workflow and
`code/commit-messages` for the message standard. Stage selectively,
draft the message in a temp file, then commit. This is a draft — the
message conveys the change's intent, and both code and message may be
amended after review.
Done when:

- the change is committed with a message drafted to `code/commit-messages` standard

**Review the change.** Run `deep-review` on the committed change: pass
the commit hash to each reviewer so they review the exact commit. The
review scope includes both the diff (correctness, conventions, plan
alignment) and the commit message (does it accurately convey the
change's intent and meet `code/commit-messages` standard). Meta-review
their findings.
Done when:

- deep-review has ended on the change (no blocking findings, or accepted-with-reason)

**Amend.** Apply review findings to the code and adjust the commit
message if the review surfaced a better framing. Stage the fixes and
`git commit --amend` to fold them into the draft commit. If a review
finding changes the change's shape substantially, re-review the
amended commit.
Done when:

- review findings are addressed in the code
- the commit message reflects the final change
- the amended commit is verified with `git log` and `git show --stat`

## Key behaviors to remember

- **Explore before plan, plan before code.** The order exists because each
  step feeds the next — touch points from exploration go into the plan,
  the plan gates the implementation — and skipping a step means the next
  one works from incomplete information.
- **Two review checkpoints, not one.** Reviewing the plan catches design
  errors before you spend implementation effort; reviewing the change
  catches implementation errors before you ship, because each checkpoint
  targets a different class of mistake.
- **Stop if the plan breaks.** If implementation reveals the plan was
  wrong, revise the plan, re-pin it so the pinned copy matches the file,
  and re-review — do not silently improvise, because an improvised change
  has no reviewed design behind it.
- **Unpin the plan when the task is done.** A pinned plan occupies
  context and attention; when the task is complete and you are moving to
  unrelated work, unpin it (and archive the plan file to
  `/tmp/plans/archived/`), because a stale plan pinned across tasks adds
  noise without adding value. Judge the timing yourself — the plan may be
  worth keeping briefly if a follow-up task builds on it.

## Anti-patterns

- **Coding before exploring** — jumping to edits without understanding the
  codebase, because without touch points you reinvent patterns the
  codebase already provides and miss conventions you should follow.
- **Skipping plan review** — implementing an unreviewed plan, because a
  design error caught at planning costs minutes to fix but hours if
  caught after implementation.
- **Finalizing without review** — treating the draft commit as done without
  deep review, because implementation introduces bugs the plan could
  not predict and an unreviewed message may misstate the change.
- **One review instead of two** — reviewing only the plan or only the
  change, because each catches a different failure class and one cannot
  substitute for the other.
