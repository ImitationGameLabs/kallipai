---
name: Team Building
description: When you need to assemble or restructure a multi-agent team for a task — the process from domain assessment through template selection, scaling, spawning, and sustaining the team
---

# Team Building — assemble the right team

This is the process for building a team when a task warrants one. It
delegates the principles of good structure to `what-makes-a-good-team`,
the concrete roles and pipelines to domain templates (e.g.
`software-development`), and the spawn/message/cleanup mechanics to
`agent/subagent-management`. This skill owns only the ordered sequence.

## When to use

- A task is large enough to benefit from parallel review or exploration
- You are about to spawn subagents for a recurring domain and want the
  team to persist across related tasks

## When NOT to use

- For a trivial task you can do yourself, because the coordination cost
  of spawning a team exceeds the benefit — just do it.
- To look up what makes a good team structure, because that is
  `what-makes-a-good-team` (the standard); this skill only runs the
  sequence.
- To look up concrete roles for software engineering, because that is
  `software-development` (the domain template); this skill only selects
  and applies it.

## The sequence

**Assess the task domain.** Identify what kind of work this is —
software engineering, research, data analysis, or something else —
because the domain determines which template to apply. Also assess the
breadth: how many files or sources it touches, whether the codebase is
familiar, and whether the change is risky. This assessment drives both
template selection and scale.
Done when:

- the domain is named (e.g. "software engineering", "research")
- the breadth is characterized (small / medium / large) with the
  signals that support that judgment

**Select a template.** Match the domain to an available template. If a
template exists, apply it; if not, design a team from the principles in
`what-makes-a-good-team`, treating the surgical-team model as the
default: one surgeon, optional scout, reviewers.
Done when:

- a template is selected, or a custom structure is defined with
  reference to the principles it derives from
- the team-should-build vs go-solo decision is resolved: if the task is
  a single obvious step, do not build a team

**Determine scale.** Using the template's scaling rules (or the general
table in `what-makes-a-good-team`), decide how many reviewers and
whether a scout is warranted. The question is never "whether to review"
but "how many reviewers" — review is a constant. Scale the reviewer
count to the diff size and risk, not to an aspiration for parallelism.
Done when:

- the reviewer count is set (1–3) with a rationale tied to diff size
  or risk
- the scout decision is set (yes/no) with a rationale

**Spawn the team.** Spawn agents per the selected template's role
definitions, using `agent/subagent-management` for the mechanics.
Capture each agent ID immediately. Assign permissions per the template
(Normal for the surgeon, Guest for scouts and reviewers). Give each
agent a focused prompt that names its role, its scope, and your agent
ID for reporting back.
Done when:

- all team agents are spawned with their IDs captured
- each agent knows its role, its boundaries, and how to report back
- the surgeon holds the workspace dirlock and no other agent has write
  access to the codebase

**Run the pipeline.** Execute the template's phases — exploration
(scout, if spawned), implementation (surgeon, serial), review
(reviewers, parallel). The surgeon synthesizes review findings and
fixes what is justified; the review-fix loop's termination rules (Pass,
Accept-with-reason, Escalate) live in `skill/reviewing-a-skill`.
Done when:

- the task is complete and the diff has passed review

**Sustain or clean up.** If this domain has recurring tasks ahead, keep
the surgeon and reviewers alive — they have accumulated project context
that a fresh team would lack. Remove the scout (it is disposable). If
the domain is done, remove all agents and release resources. A
specialized team should persist across related tasks, not be rebuilt
each time.
Done when:

- persistent agents (surgeon, reviewers) are kept for the next task, or
  all agents are removed if the domain is complete
- the scout is removed
- workspace dirlocks are correct (held by persistent agents, released
  by removed ones)

## Key behaviors to remember

- **Template-first, standard-fallback** — when a domain template exists,
  apply it rather than improvising, because the template encodes
  battle-tested role definitions and scaling rules; fall back to the
  principles in `what-makes-a-good-team` only when no template matches.
- **Scale is a continuum, not tiers** — the small/medium/large labels
  are shorthand for a continuous judgment about reviewer count and scout
  necessity, because rigid tier boundaries lead to over- or
  under-staffing at the edges; treat the labels as guidance, not gates.
- **One surgeon, always** — regardless of template, exactly one agent
  holds the edit lock and the mental model, because parallel editing
  causes conflicts and fragments the design; this is invariant across all
  templates.
- **Review is non-negotiable** — every diff gets at least one reviewer,
  because the question is how many reviewers, not whether to review; a
  team without reviewers is incomplete by definition.

## Anti-patterns

- **Building a team for a trivial task** — spawning agents for a one-file
  fix you could do yourself, because the coordination cost exceeds the
  benefit; assess breadth first and skip the team when the task is small.
- **Improvising when a template exists** — designing a custom structure
  for software engineering instead of applying `software-development`,
  because the template encodes tested role definitions and scaling rules;
  use the template.
- **Rebuilding between tasks** — removing a software team after one task
  and respawning for the next, because you discard accumulated context
  (conventions, pitfalls, review patterns) and re-pay setup cost; keep
  specialized teams stable across related tasks.
