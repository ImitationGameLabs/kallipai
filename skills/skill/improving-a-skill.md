---
name: Improving a Skill
description: When you need to change an existing skill — a fix, an optimization, or catching up to a changed standard — without breaking dependents, ending in re-review
---

# Improving a Skill — change a working skill without breaking what depends on it

An existing skill is a node other skills reference and the index matches, so
changing it has a blast radius creating never has. This workflow diagnoses the
reason, keeps the contract, and re-reviews. The content standard lives in
`skill/what-makes-a-good-skill`; the final review delegates to
`skill/reviewing-a-skill`.

## When to use

Load this when you are modifying, fixing, or optimizing an existing skill —
including bringing it up to a standard that has changed (run this on each
affected skill). Simple fixes (a typo, a wording tweak in the body) still run
the full sequence, review included — the pre-review gates pass fast when the
blast radius is empty, but the Review step runs its full check, so the sequence
still ends in review.

## When NOT to use

- To create a skill from scratch — use `skill/creating-a-skill`.
- To revise your own first draft before it is referenced — use
  `skill/creating-a-skill`'s review-revise loop; the blast radius is empty until
  the skill is a node.
- To verify a skill without changing it — use `skill/reviewing-a-skill`.
- To consult the content standard — use `skill/what-makes-a-good-skill`. This
  skill only runs the change workflow.

## The sequence

**Diagnose.** Read the skill and `skill/what-makes-a-good-skill`; state precisely
what is wrong or suboptimal and why, because a change without a root cause treats
the symptom and the problem returns.
Done when:

- there is a precise change rationale, rooted in the standard or a concrete observed failure

**Check the blast radius.** Find what depends on the current contract, because a
skill is a node: other skills reference it by path-id, the index matches its
description, and siblings route to it. Grep for references to this skill and note
its index contract (name, description, filename) and its routing.
If the change adds new content, also check whether it overlaps with an
existing skill or introduces a concept another skill should reference —
forward interconnections matter as much as the backward blast radius.
Done when:

- every dependent this change would touch is listed — cross-references, the index contract, and sibling routing
- if the change adds content, forward interconnections are checked too (overlaps deduped, new reference points linked)

**Decide the change, keeping the contract.** Scope the change keeping the
archetype spine intact, because drifting it mid-skill breaks the structure the
devices hang on. A rename, move, archetype switch, or description-trigger reword
is high-risk — a trigger break is functionally a rename. Re-check placement and
granularity against `skill/skill-management`, because a change can push a skill
past the split threshold or into the wrong category.
Done when:

- the change is scoped, the archetype is intact (or intentionally switched), and placement and granularity are re-checked

**Make the change.** Make the smallest change that resolves the diagnosed root
cause, in reasoned voice with path-id cross-references. A high-risk change goes
in its own revertable commit, separate from content tweaks. Update every
dependent identified above, because a stale reference or broken route is the one
thing a review of this file alone will not catch.
Done when:

- a grep for this skill's path-id returns no stale references, and every dependent from the blast-radius step is updated

**Review it.** Hand the changed skill to `skill/reviewing-a-skill`
immediately after making the change, before the skill ships or is used —
it re-checks end to end against the standard, because an improvement
that fails the standard is a regression. If it requests changes, fix and
re-review; the loop's termination and convergence rules live in that
skill.
Done when:

- the review-fix loop in `skill/reviewing-a-skill` has ended on this skill (Pass, or a finding accepted-with-reason), and the symptom diagnosed in step 1 no longer holds

## Anti-patterns

- **Changing without diagnosing** — a tweak aimed at a symptom lets the root
  cause recur, because the real fault sits upstream of what you changed;
  diagnose first.
- **Breaking the contract silently** — changing the name, description, filename,
  or routing without updating the skills that reference it orphans a sibling or
  stops the skill loading, because those depend on the old contract; update them.
- **Drifting the archetype** — a change that half-converts a Reference into a
  Process leaves the skill carrying neither device, because the spine is what the
  devices hang on; switch archetypes deliberately or not at all.
- **Optimizing against the standard** — an "improvement" that violates
  `skill/what-makes-a-good-skill` (a numbered list, a bare `Never`) is not an
  improvement, because the standard is what "better" is measured against; measure
  the change against the standard first.
- **Skipping the review on a "small" change** — the change-step focuses on the
  line you touched and does not re-check the rest of the skill against the
  standard, because review is the only end-to-end pass; still hand it to
  `skill/reviewing-a-skill`, however small.
