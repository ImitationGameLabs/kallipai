---
name: Deep Review
description: When you need a thorough review of a plan, code change, or artifact — spawn multiple independent reviewers in parallel, collect their findings, and meta-review the results to produce a verified, deduplicated set of findings
---

# Deep Review — parallel independent review with meta-review

A single reviewer has blind spots. Deep review spawns multiple independent
subagents, each reviewing the same target fresh, then collects and
meta-reviews their findings — verifying each one against the target,
deduplicating overlaps, and judging whether each is worth acting on. The
output is a single, trusted findings list the requester can act on with
confidence.

## When to use

- After writing a plan, before implementing it
- After drafting a commit (the change is committed but not yet finalized), to review the diff and commit message before amending
- Any artifact where a missed issue has real cost and a single pass is not enough

## When NOT to use

- Quick self-review of a trivial change (a one-line typo fix), because the
  overhead of spawning reviewers exceeds the value
- Reviewing a skill file — that is `skill/reviewing-a-skill` (specialized
  for skill standards); use deep-review only if the skill review itself
  needs multiple independent passes

## The sequence

**Frame the review target.** State precisely what is being reviewed —
the file paths, the plan text (`/tmp/plans/<short-name>.md`), the diff range,
or the commit hash — and what standard it should be checked against. Pass
to each reviewer so they read the right file without searching. Each
reviewer needs the same target and the same success criteria, because
divergent framings produce findings that cannot be reconciled.
When reviewing a plan, check its `Status` line — review plans that
are `draft` or `in-progress`. If the plan is `done` or archived,
report back to the requester that the plan is already complete,
because reviewing a finished plan wastes effort on work that shipped.

When reviewing a committed change, pass the commit hash to each reviewer
so they review the exact commit (`git show <hash>`); include the commit
message in the review scope, because the message states the change's intent
and reviewing it catches mismatches between intent and implementation, as
well as format violations against `code/commit-messages`.
When the target is a code change, the review standard is
`code/reviewing-code` — what to check, in what priority order, and what
blocks.

Done when:

- the target is precisely named (file paths, diff, plan text, or commit hash)
- the applicable standard or criteria is stated (a skill, a spec, or a checklist)
- if the target is a plan, its Status is `draft` or `in-progress`

**Spawn N independent reviewers.** Spawn N subagents, each with the
review target, the standard, and the instruction to review independently
and report findings. Use `agent/subagent-management` for the spawn pattern.
Parallelize — spawn all reviewers, then collect as each reports back,
because serial review wastes wall-clock time.

The reviewer count depends on the change's risk and scope, because the
marginal value of each additional reviewer drops sharply after 2:

- **2 reviewers** (default) — covers most changes; two independent
  perspectives catch most blind spots while keeping budget cost moderate.
- **3 reviewers** — high-risk or multi-file changes where a third
  perspective's coverage justifies the cost (architecture changes, core
  logic, security-sensitive code).
- **1 reviewer** — only when the change is simple enough that deep-review
  may be overkill; consider whether a single `skill/reviewing-a-skill`
  pass or self-review suffices instead.
Done when:
- N reviewers are spawned (2 default, 3 for high-risk), each with the same target and standard
- each has the delivery rule: report via `kallip message <your-id>` before break

**Collect findings.** Wait for each reviewer's report. As reports arrive,
accumulate them without filtering — do not pre-judge a finding before the
meta-review step, because premature filtering drops findings a later
reviewer might corroborate.
Done when:

- all spawned reviewers have reported, or a timeout is reached with at least 2 reports collected (1 if only 1 was spawned)

**Meta-review.** Read all findings together. For each finding:

- Verify it against the actual target (re-read the file, diff, or plan) —
  a reviewer may misread or hallucinate.
- Deduplicate — if two reviewers found the same issue, merge into one.
- Judge value — is it blocking, important, or minor? Does it apply, or is
  it out of scope or context-blind?
- Accept or reject each finding with a reason. "Accept" means it goes into
  the final findings list; "reject" means it was checked and found inapplicable.
Done when:
- every finding is verified against the target, deduplicated, and accepted or rejected with a reason
- the final findings list names each accepted finding with its severity (blocking / important / minor)

**Deliver findings.** Send the final, verified findings list to the
requester (yourself, if you initiated the review). If no findings survive
meta-review, state that explicitly — silence is ambiguous.
Done when:

- the findings list (or clean confirmation) is delivered

## Key behaviors to remember

- **Same target, same standard, independent eyes.** Every reviewer gets the
  same frame, because divergent framings produce irreconcilable findings;
  but each reviews independently, because the value is in their distinct
  blind spots.
- **Meta-review is the load-bearing step.** Collecting raw findings without
  verifying them is worse than a single careful review, because unverified
  findings create noise and false confidence; the meta-review is where the
  signal is separated.
- **Verify, don't trust.** Re-read the target for each finding, because
  reviewers can misread or hallucinate — a finding that sounds plausible
  but does not match the actual artifact is a false positive.
- **Reject with a reason.** A rejected finding should state why it does not
  apply, because the reason is the audit trail that prevents the same
  false positive from recurring.

## Anti-patterns

- **Skipping meta-review** — forwarding raw reviewer findings without
  verification, because unverified findings include hallucinations and
  duplicates that waste the requester's effort on non-issues.
- **Serial review** — spawning one reviewer, waiting, then spawning the
  next, because parallel spawning cuts wall-clock time in half or more
  for no cost in quality.
- **Different framings per reviewer** — telling each reviewer a different
  angle or standard, because the findings cannot be reconciled or
  deduplicated across incompatible criteria.
- **Pre-filtering during collection** — dropping a finding before
  meta-review because it "sounds wrong", because premature filtering
  discards findings another reviewer might corroborate.
