---
name: Reviewing a Skill
description: When you are reviewing or auditing a skill against the standard, before approving it
---

# Reviewing a Skill — checks against the standard

This is the verification workflow for a skill — after creation or on revision.
It checks the skill against the standard (`skill/what-makes-a-good-skill`) and
the system guidance (`skill/skill-management`), and runs an independent review.

## When to use

Load this when you are reviewing a skill — your own draft or a revision.

## When NOT to use

- To create a skill from scratch — use `skill/creating-a-skill` (its final step
  delegates here).
- To consult the standard (what makes a good description, the archetype
  templates, the voice rules) — use `skill/what-makes-a-good-skill`. This skill
  only verifies.
- To improve or fix a skill you have reviewed — use `skill/improving-a-skill`.

## The sequence

**Load the standard and system guidance, then read the skill fresh.** Read
`skill/what-makes-a-good-skill` and `skill/skill-management`, then the skill
under review.
Done when:

- all three are in context, and you are reading the skill as a reader would, not as the author

**Check the index entry.** Run `kallip skill index <skills-path>` and read the
skill's line alongside its siblings.
Done when:

- the description reads as a trigger (opens with `When`/`How to`, or for a prerequisite skill, the imperative itself; names the task shape)
- the description states when to reach for the skill, not a summary of its internal steps
- if the skill has a required prerequisite, the description embeds that imperative into the trigger, because a trigger that omits it gets matched but the first step gets skipped
- the description is one line
- it is distinguishable from sibling skills

**Check the archetype and its devices.** Confirm the skill committed to one
archetype and carries that archetype's devices, because a skill that never
commits carries neither gates nor `*Avoid:`.
Done when:

- exactly one archetype fits, and its devices are present — gates on each non-obvious step for Process, `*Avoid:` + decision rules for Reference
- a Wrapper defers to a `--reference`/`--skill` command and adds only when/why

**Check voice and references.** Reasoned voice keeps a skill from being followed
blindly, and clean refs keep it routable — doctrine invites blind adherence and
a stale path-id breaks loading.
Done when:

- the voice is reasoned (because-clauses; bare `Never` only for genuine footguns)
- cross-references are path-id (no `.md`), and frontmatter is exactly `name` + `description`
- the filename is kebab-case and names the domain, not the tool

**Check placement and granularity.** Is the skill in the right location, and is
it the right size? Judge against `skill/skill-management`'s "Where to place it
(including its category table)" and "When to split / When to merge" — because a
skill in the wrong category or at the wrong granularity hurts discoverability and
breeds overlap.
Done when:

- the location matches the placement guidance (root for cross-cutting primitives, else the matching category; no new category unless ~6-8 skills, or the self-referential `skill/` domain) and no deeper than `category/skill.md`
- it passes the split/merge tests — not one skill covering multiple distinct scenarios exceeding ~80 lines, or carrying a section that is dead weight when pinned; and not tightly coupled to an obvious sibling, co-loaded whenever either is needed, and under ~30 lines

**Check for sibling patterns to borrow.** Scan sibling skills for a structural
pattern worth borrowing, because the library has already solved problems you are
re-solving — e.g., whether this skill belongs in a workflow + standard pair.
Done when:

- you have checked the sibling skills for a pattern that would improve this one, and adopted or rejected it with a reason

**Run an independent review.** Independent reviewers catch what the author
cannot; run multiple when available, integrate their findings, and reconcile
disagreements.
Done when:

- findings are resolved, or accepted-with-reason (a loop exit — see "The review-fix loop" below)

Outcome: approve, or request-changes naming the gates that failed.

## The review-fix loop

Request-changes triggers a fix, and the fix is re-reviewed, because a fix can
introduce a new problem — so the author iterates review, fix, re-review. The loop
has defined exits and guards against oscillation; this is what "re-run until it
passes" actually means.

The loop ends when any exit holds:

- **Pass** — a review returns approve.
- **Accept-with-reason** — a remaining finding is judged not load-bearing and
  accepted with a recorded reason. This is the primary escape: not every finding
  must be fixed.
- **Escalate** — stop and take a non-converging loop to the user.

Two guards stop oscillation:

- **Scope contraction** — each round fixes only the confirmed findings from the
  previous review; a genuinely new, out-of-scope issue is deferred to a separate
  change, because expanding the frontier every round is what makes review endless.
- **Convergence, not regression** — if a fix reintroduces a framing a prior round
  moved away from, or reviewers disagree irreconcilably, that is an open decision,
  not a fix; resolve it via Accept-with-reason or Escalate, because oscillating
  between two valid framings never converges.

After 2-3 rounds without convergence, invoke Accept-with-reason or Escalate,
because by then the remaining findings reflect a values or tradeoff difference,
not a factual gap another round will settle. The valve that does most
of the work is Accept-with-reason: most "endless review" comes from treating
every finding as must-fix.

## Anti-patterns

- **Reviewing as the author, not a reader** — you cannot judge whether a
  description will be matched when you already know what it means; read it fresh
  or hand it to an independent reviewer.
