---
name: Software Development Team
description: When you are running a software-engineering task and want the concrete surgeon/scout/reviewer team template — roles, pipeline, and scaling — that applies the surgical-team model to engineering work
---

# Software Development Team — Reference

A domain template that applies the surgical-team principles
(`what-makes-a-good-team`) to software engineering. It defines a stable
team structure — one surgeon who edits, optional scouts who explore,
and reviewers who verify — with concrete roles, permissions, pipeline
phases, and scaling rules. For the spawn/message/cleanup mechanics, see
`agent/subagent-management`.

## The team at a glance

| Role     | Count | Permission | Writes code | Persistent | Purpose                                   |
| -------- | ----- | ---------- | ----------- | ---------- | ----------------------------------------- |
| Surgeon  | 1     | Normal     | Yes (sole)  | Yes        | Decides, plans, implements, integrates    |
| Scout    | 0–1   | Guest      | No          | No         | Maps the codebase before the surgeon acts |
| Reviewer | 1–3   | Guest      | No          | Yes        | Independent review of each diff           |

The surgeon is the root agent itself, or a lead subagent the root
delegates to. Either way, one agent holds the edit lock and the complete
mental model — there is never a second writer.

## Roles

### Surgeon — the sole editor and decision-maker

The surgeon owns the task end to end: framing, planning, implementing,
and integrating feedback. It holds the workspace dirlock, is the only
agent that writes to the codebase, and makes all design decisions. When
the root agent is the surgeon, it may still delegate sub-tasks — but it
does not surrender the edit lock or the decision authority.

*Avoid:* splitting implementation across two surgeons (one does the
frontend, another the backend), because two writers cannot maintain a
shared mental model in real time and the integration cost erases the
parallel gain; have one surgeon do all editing, even if slower.

### Scout — the disposable explorer

A scout maps the codebase before the surgeon touches it: touchpoints,
dependencies, conventions, and pitfalls. It is a Guest (read-only),
reports findings to the surgeon via message or a file in `/tmp`, and is
removed after reporting — it is not a persistent role.

*Avoid:* keeping a scout alive after it has reported, because it
consumes context budget for no further value; remove it once the
surgeon has absorbed the findings.

### Reviewer — the independent verifier

Reviewers are read-only (Guest) agents that examine the surgeon's diff
from an independent perspective. They carry no sunk cost in the code —
they did not write it — so they are free of the confirmation bias that
the surgeon has toward its own implementation. Deploy 1–3 reviewers per
diff, splitting by dimension (correctness, edge cases, API design) or by
module when the diff is large. Reviewers are persistent: the same
reviewers accumulate project-specific knowledge across diffs.

*Avoid:* asking the surgeon to review its own diff as a substitute for
independent reviewers, because self-review reproduces the same blind
spots that produced the code; the value of review is the fresh
perspective, which the surgeon structurally cannot provide.

## The pipeline: serial editing, parallel review

The team moves through three phases. Editing is always serial; review is
always parallel.

```text
Phase 1: Exploration        Phase 2: Implementation      Phase 3: Review
─────────────────────       ────────────────────────      ──────────────
Scout maps codebase    →    Surgeon implements      →    Reviewers (1-3)
(parallel, read-only)       (serial, sole writer)        (parallel, read-only)
       │                           │                            │
       ▼                           ▼                            ▼
  findings → surgeon          diff ready              findings → surgeon
                                                          → fix → re-review
```

**Phase 1 — Exploration.** If the task requires understanding unfamiliar
code, spawn a scout. The scout reads broadly, maps touchpoints and
dependencies, and reports. The surgeon waits for the report or uses the
time to frame and plan. Skip the scout for small fixes where the
surgeon already knows the code.

**Phase 2 — Implementation.** The surgeon implements the change. This is
strictly serial — one writer, no parallel editing. The surgeon may
consult `planning` for the implementation plan.

**Phase 3 — Review.** Spawn or reuse 1–3 reviewers, each with a focused
brief. They review in parallel and report findings. The surgeon
synthesizes the feedback, fixes what is justified, and re-submits for
re-review if the changes are substantive. The review-fix loop's
termination rules (Pass, Accept-with-reason, Escalate) live in
`skill/reviewing-a-skill`.

**Pipeline overlap.** The surgeon may begin implementing sub-task A
while a scout explores sub-task B — but only when the surgeon
consciously manages the handoff, because a scout's findings arriving
mid-implementation can invalidate assumptions. Do not overlap by
default; overlap when the sub-tasks are genuinely independent.

## Scaling rules

For how reviewer count and scout necessity scale with task breadth, use
the table in `what-makes-a-good-team` — this template adds the
SE-specific split strategy on top:

- **Small diff (1–2 files):** one reviewer, general review — no split
  needed.
- **Medium diff (3–8 files):** 1–2 reviewers, split by dimension —
  e.g. one for correctness and logic, another for edge cases and error
  handling.
- **Large diff (9+ files):** 2–3 reviewers, split by module or dimension
  or both — e.g. one per subsystem touched, or one for API design and
  one for implementation correctness.

*Avoid:* deploying three reviewers on a one-file fix, because the
briefing and synthesis cost exceeds the parallel benefit; and deploying
one reviewer on a 30-file change, because a single reviewer's attention
thins across a large surface and misses issues a split would catch.

## Permissions and workspace

- **Surgeon:** Normal class, holds the workspace dirlock. If the surgeon
  is a lead subagent, its `--workspace-root` must be within the root
  agent's workspace.
- **Scout:** Guest class (`--permission-class guest`), read-only. It
  writes findings to `/tmp` or reports via message; it has no workspace
  dirlock.
- **Reviewer:** Guest class (`--permission-class guest`), read-only. It
  reads the diff from the surgeon's workspace (bind-mounted read-only
  under the Guest's readonly holes) and reports via message.

For the workspace nesting and dirlock rules that govern these
configurations, see `agent/subagent-management`.

## Coordination patterns

- **Scout → Surgeon:** The scout reports findings by message (short) or
  by writing to `/tmp/<task>-scout.md` and sending the path (long). The
  surgeon absorbs the findings, then removes the scout.
- **Reviewer → Surgeon:** Each reviewer messages its findings to the
  surgeon. For substantial reviews, the reviewer writes to
  `/tmp/<task>-review-<n>.md` and sends the path. The surgeon
  synthesizes all reviews and decides what to fix.
- **No peer-to-peer:** Scouts and reviewers never message each other.
  All information flows through the surgeon, per the convergent-flow
  principle in `what-makes-a-good-team`.

## Decision rules

- If the root agent's context budget is the bottleneck, then delegate the
  surgeon role to a lead subagent, because implementation detail consumes
  context the root needs for coordination; the lead subagent holds the
  edit lock and mental model, and reports back to the root.
- If the codebase is unfamiliar to the surgeon, then spawn a scout before
  implementing, because parallel exploration saves the surgeon turns of
  serial reading; if the surgeon already knows the code, skip the scout.
- If reviewers disagree on a finding, then the surgeon decides, because
  the surgeon holds the design context and the edit authority; the
  review-fix loop's Accept-with-reason exit applies when the disagreement
  reflects a values difference, not a factual gap.
- If the same project has sequential tasks, then keep the surgeon and
  reviewers alive between tasks, because they have accumulated project
  context (conventions, pitfalls, review patterns) that a fresh team
  would lack; only the scout is disposable.

## Anti-patterns

- **Parallel editing** — two agents writing to the same codebase at once,
  because merge conflicts and mental-model fragmentation cost more than
  the parallelism saves; editing is the surgeon's alone.
- **Skipping review on small diffs** — treating a one-line fix as too
  simple to review, because a one-liner can break as much as a refactor
  and the review cost is minimal; review is a constant.
- **Persistent scouts** — keeping a scout alive after it has reported,
  because it occupies a slot and context budget for no further value;
  remove scouts once the surgeon has absorbed their findings.
- **Reviewer as co-author** — asking a reviewer to fix the issues it
  finds, because that contaminates the independent perspective that makes
  review valuable; the surgeon fixes, the reviewer re-verifies.
- **Flat team on large tasks** — one surgeon with no scout on a 30-file
  change, because the surgeon spends turns exploring that a scout could
  have done in parallel; add a scout when the exploration is non-trivial.
