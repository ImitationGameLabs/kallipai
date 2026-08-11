---
name: Reviewing Code
description: When you are reviewing or auditing a code change against the standard — what to check, in what priority order, and what blocks versus what advises
---

# Reviewing Code — Reference

A code review is not a style audit. Its highest-value work is finding
correctness bugs — edge cases, error paths, wrong logic — that the author
missed. Style and naming issues are easy to spot in a diff but are the least
important findings, because a style-perfect bug ships faster than a messy
fix. This skill is the standard for what to check when reviewing a code
change, in what priority order, and what blocks. It provides the craft that
`deep-review`'s reviewer subagents apply when reviewing a diff or commit.

## Priority order: correctness first

Review in this order, because findings earlier in the list are more
important and should be resolved before spending effort on later categories:

1. **Correctness** — Does the logic do what it claims? This is the only
   category that can block.
2. **Tests** — Does the change include regression tests for new behavior?
3. **Clarity** — Will a reader who didn't write it understand it?
4. **Style** — Does it match existing conventions?

Working top-down ensures that a style suggestion does not distract from an
unaddressed correctness bug in the same diff, because a review that surfaces
only minor findings while a logic bug sits unflagged has failed.

*Avoid:* reviewing bottom-up (style first), because surface issues are easy
to spot in a diff and crowd out the logic analysis that actually prevents
bugs; read the diff for correctness before commenting on style.

## Correctness — what to check

Correctness findings are the ones worth blocking on. Check for:

- **Edge cases** — empty input, zero, single-element, maximum, negative. A
  function that handles the common case but crashes on the boundary is a
  correctness bug.
- **Error paths** — what happens when a dependency fails? Does the code
  handle errors, or does it `unwrap`/panic on a path that can fail in
  production?
- **Logic errors** — off-by-one, inverted condition, wrong operator. Read
  the logic, don't just scan for patterns.
- **Resource management** — are files, connections, locks released on all
  paths, including error paths?
- **Side effects** — does the change introduce a side effect the caller does
  not expect? A function that previously had no side effects gaining one is
  a breaking change even if the signature is unchanged.

If you cannot explain why the code is correct for the edge cases above, that
is itself a finding — an unconfirmed-correct change is not an approved
change, because "looks right" is not a correctness argument.

*Avoid:* approving code you cannot trace through the logic, because
pattern-matching to a familiar shape feels productive but skips the
verification that confirms correctness; if you cannot trace it, say so.

## Tests — does the change guard new behavior?

A change that adds behavior should add tests that would fail without it.
Check:

- Does the change include tests for the new behavior, or only for the
  implementation?
- Do the tests follow `code/testing` (behavior not implementation,
  regression value)?
- Are the edge cases from the correctness check tested?

Missing tests for non-trivial new behavior is an important finding. Missing
tests for a trivial pass-through (a one-line delegation) is not, because a
pass-through cannot regress in the author's logic — see `code/testing`'s
decision rules.

## Clarity — will the next reader understand it?

Clarity findings are advisory. Check:

- **Naming** — do names describe what the code does, or how it does it?
- **Hidden complexity** — is there a non-obvious operation behind a
  simple-looking call? If the code is unclear because it is too complex,
  that is a `code/complexity-control` finding, not a naming suggestion.
- **Misleading structure** — does the code's structure suggest a behavior it
  does not have?

Clarity findings should suggest, not block — unless the code would actively
mislead a future reader into introducing a bug, because that is a
correctness risk deferred.

## Decision rules

- If a code path does not handle an edge case that can occur in production,
  it is a correctness bug — block, because unhandled edge cases are the most
  common source of runtime failures.
- If an `unwrap` or `expect` is on a path that can fail in production, block
  — because it will panic instead of handling the error.
- If the change adds non-trivial behavior without a test that would fail
  without it, flag as important — because untested behavior has no
  regression protection.
- If you cannot confirm correctness (the logic is too complex to trace in
  review), say so — because "I cannot verify this" is a more honest finding
  than "looks good."
- If the commit message does not accurately describe what the change does,
  flag as important — because a misleading message causes future debugging
  confusion; check against `code/commit-messages`.
- If the code compiles and handles edge cases but the naming is unclear,
  advise — because clarity improvements are valuable but not urgent.
- If a finding is purely stylistic, mention it only if trivial to fix —
  because style is advisory and blocking on it turns review into
  bikeshedding.

## Anti-patterns

- **Rubber-stamping** — approving a change you did not trace through the
  logic, because "looks right" is not a correctness argument; if you cannot
  explain why the code handles the edge cases, say so rather than approving.
- **Style-fixation** — spending the review on naming and formatting while
  correctness goes unchecked, because surface issues are easy to spot in a
  diff but are the lowest-value findings; start with correctness.
- **Approving locally, missing globally** — confirming each function in
  isolation without checking whether the change fits the overall design,
  because a diff can be locally correct and globally wrong (a new parameter
  that should have been a new type, a fix that papers over a design flaw).
- **Blocking on style** — refusing to approve a correct change over a
  formatting preference, because style is advisory and blocking on it turns
  review into bikeshedding; suggest and move on.
