---
name: Creating a Skill
description: When you are about to create a new skill from scratch and want the end-to-end workflow
---

# Creating a Skill — from deciding to reviewing

This is the end-to-end workflow for making a new skill from scratch. Each step
delegates the craft to `skill/what-makes-a-good-skill` (the standard) and the
lifecycle to `skill/skill-management`; this skill owns only the order and the
review.

## When to use

Load this when you are about to create a new skill file and want the ordered workflow.

## When NOT to use

- To decide *whether* a skill is worth creating, or to find/organize existing
  skills — that is `skill/skill-management`.
- To consult the standard mid-write (what makes a good description, the
  archetype templates, the voice rules) — that is
  `skill/what-makes-a-good-skill`. This skill is only the end-to-end sequence.
- To review an existing skill — that is `skill/reviewing-a-skill` (this skill's
  final step delegates there).
- To improve or fix an existing skill — use `skill/improving-a-skill`.

## The sequence

**Confirm it earns a skill.** Apply the "does this teach something
`--help` or official docs don't?" test from `skill/skill-management`'s "What
makes a good skill" — cross-ref it, don't restate it here.
Done when:

- the skill passes that test, and you've hit the pattern 2-3 times (or it's
  clearly worth preserving even if fresh)

**Write the index entry first.** Choose the filename (the programmatic id) and
write the `name` + `description` pair per `skill/what-makes-a-good-skill`.
Done when:

- the description opens with a trigger verb (`When`/`How to`)
- it names the task shape, not just the topic
- it is one line, and you believe the agent would match it at index time

**Commit to an archetype.** Process, Reference, or Wrapper, via the decision
rule in `skill/what-makes-a-good-skill`. If the domain also has a body of
criteria, consider the workflow + standard pair (see `skill/what-makes-a-good-skill`).
Done when:

- exactly one archetype fits by the rule
- you can name why each of the others fits worse

**Fill the body from the template.** Apply that archetype's devices — gates on
each non-obvious step for Process, `*Avoid:` + decision rules for Reference.
Done when:

- the archetype's devices are present
- the voice is reasoned throughout (because-clauses; bare `Never` only for
  genuine footguns)

**Review it.** Hand the finished skill to the `skill/reviewing-a-skill` workflow
— the checks live there, so they are not restated here. If it requests changes,
fix and re-review; the loop's termination and convergence rules live in that
skill.
Done when:

- the review-fix loop in `skill/reviewing-a-skill` has ended on this skill (Pass, or a finding accepted-with-reason)

## Anti-patterns

- **Skipping the independent review** — that step is the one most likely to catch
  a description that won't load or a ceremonial gate; without it the whole
  sequence collapses to a draft.
