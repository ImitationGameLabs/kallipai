---
name: What Makes a Good Team
description: When you are designing or judging a multi-agent team structure — the surgical-team principles that make coordination effective vs the costs that make it fail
---

# What Makes a Good Team — Reference

The surgical-team model for multi-agent coordination: one decision-maker
holds the complete mental model, assistants feed or verify it, and
parallelism is concentrated where it is safe (review, exploration) rather
than where it is destructive (concurrent editing). This skill defines the
principles; domain templates (e.g. `software-development`) apply them to
concrete work, and `agent/subagent-management` provides the
spawn/message/cleanup mechanics.

## The surgical-team principle

A good team has exactly one agent who owns the decision and the core work
— the surgeon. Every other agent either feeds the surgeon (scouts,
researchers) or verifies the surgeon's output (reviewers). The surgeon
holds the complete mental model of the task; no one else needs to, because
partial context flows toward the surgeon, not sideways.

This is not egalitarian. An egalitarian team — where agents share editing,
negotiate decisions, and sync state — pays coordination tax on every turn
without gaining throughput, because the bottleneck in knowledge work is
coherence, not raw capacity.

*Avoid:* flat peer teams where multiple agents edit the same codebase
independently, because concurrent edits cause merge conflicts, fragment
the mental model across agents, and produce inconsistent design decisions
— the surgeon model avoids this by making editing serial and exclusive.

## Convergent information flow

Information in a good team flows toward a single point — the surgeon — not
in a mesh between peers. Scouts report to the surgeon; reviewers report to
the surgeon; the surgeon decides. Agents do not need to sync with each
other, because there is no decision they need to make jointly.

*Avoid:* peer-to-peer messaging between assistants (scout talking directly
to reviewer), because it creates synchronization dependencies the surgeon
does not control and cannot verify — route all findings through the
surgeon instead.

## Parallelism: review is the safe seam

Not all phases benefit from parallel agents, and getting this wrong is the
most common team-design failure. The rule:

- **Editing is serial.** One writer per codebase, always the surgeon.
  Parallel editing looks productive but causes conflicts and design
  fragmentation, because two agents editing the same module cannot share
  a mental model in real time.
- **Review is parallel.** Reviewers are read-only (Guest permission), carry
  no sunk cost in the code, and can split by dimension or module with zero
  conflict risk. This is where multi-agent parallelism pays off most —
  multiple independent perspectives at no coordination cost.
- **Exploration is parallel.** A scout can map the codebase while the
  surgeon plans, because reading is conflict-free.

## Review is a constant, not a variable

Every diff gets reviewed, regardless of size. Treating small fixes as "too
simple to review" trades a small time saving for regression risk, because a
one-line change can break as much as a refactor. The decision is not
whether to review but how many reviewers to deploy — a continuum driven by
diff size and risk, not a binary gate.

## Scale matches task complexity

Team size is a function of the work's breadth, not an aspiration. More
agents means more coordination overhead — briefing, messaging, waiting —
that must be earned back by parallel throughput.

| Task shape     | Scout | Reviewers | Rationale                                              |
| -------------- | ----- | --------- | ------------------------------------------------------ |
| Small fix      | No    | 1         | One reviewer catches regressions; scout overhead unjustified |
| Medium feature | Maybe | 1–2       | Scout if exploration is non-trivial; reviewers by dimension |
| Large change   | Yes   | 2–3       | Scout maps the territory; reviewers split by module + dimension |

*Avoid:* over-staffing a small task (three agents for a one-file fix),
because spawning, briefing, and waiting cost more than the parallelism
earns; and under-staffing a large task (no scout on a 50-file change),
because the surgeon spends turns exploring that a scout could have done in
parallel.

## Team stability

A specialized team should persist across related tasks in its domain, not be
rebuilt each time. A software team that has reviewed three diffs has
accumulated context — codebase conventions, known pitfalls, review patterns
— that a freshly spawned team lacks. Rebuilding loses this investment and
re-pays the ramp-up cost.

*Avoid:* dismantling a team after each task and rebuilding for the next
similar task, because you discard accumulated context and re-pay setup cost;
keep the team alive and feed it sequential tasks in its domain.

## Decision rules

- If the task touches one file and the change is obvious, then the surgeon
  works with one reviewer and no scout, because the coordination cost of
  additional agents exceeds their benefit.
- If the task spans multiple modules or requires understanding unfamiliar
  code, then add a scout, because parallel exploration saves the surgeon
  turns of reading it would otherwise spend serially.
- If the diff is large or touches risky logic, then deploy 2–3 reviewers
  split by dimension or module, because a single reviewer's attention thins
  across a large surface and multiple perspectives catch what one misses.
- If the root agent is the surgeon, it may delegate implementation to a lead
  subagent who becomes the surgeon for that task, because the root agent's
  context is better spent on coordination than implementation detail — but
  the lead subagent then holds the sole edit lock and the mental model.

## Anti-patterns

- **Parallel editing** — multiple agents writing to the same codebase
  concurrently, because it causes merge conflicts and fragments the mental
  model; make editing serial with a single surgeon.
- **Democratic decisions** — agents voting or negotiating on design choices,
  because there is no one to break ties and the coordination cost is pure
  overhead; one agent decides.
- **Deep nesting** — surgeon → sub-surgeon → sub-sub-surgeon, because each
  layer adds message latency and context loss; keep the hierarchy shallow
  (surgeon + direct assistants).
- **Mesh communication** — assistants syncing with each other rather than
  reporting to the surgeon, because it creates uncontrolled dependencies; all
  information converges on the decision-maker.
- **Rebuilding teams per task** — spawning and removing agents for each
  individual task in a recurring domain, because it discards accumulated
  context and re-pays setup cost; keep specialized teams stable.
