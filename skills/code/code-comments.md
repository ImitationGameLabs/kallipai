---
name: Code Comments
description: When you are writing, modifying, or reviewing code — the principles for writing comments that add value rather than noise: why over what, doc comments vs internal comments, and the disciplined use of marker annotations
---

# Code Comments — Reference

Code comments are the most common thing an agent writes badly, because
the easy instinct — narrate what the code does — produces the least
useful comments. The code already says what it does; a comment that
repeats it adds length without understanding. This Reference defines
what a comment should carry instead, and distinguishes the two
situations where the instinct changes: API documentation vs internal
reasoning.

## Doc comments vs internal comments

Two kinds of comment exist, with different readers and different rules.
Confusing them is the root of most bad commenting.

**Doc comments** (`///` in Rust, `/** */` in many languages) are API
surface. Their reader is the *caller* — someone who will use the function
or type without reading its body. They document **what**: what the
function does, what the arguments mean, what it returns, when it errors
or panics. This is the one place where describing behavior is the job,
because the caller cannot (and should not) infer it from the
implementation. A pub function without a doc comment is an undocumented
API.

**Internal comments** (`//`) are for the *maintainer* — someone who is
reading or changing the body. Their job is **why**: the reasoning a
future reader cannot reconstruct from the diff. The code says what
happens; the comment says why this approach, why this edge case matters,
why this looks wrong but is actually correct.

*Avoid:* applying the "don't write what" rule to doc comments, because
doc comments exist precisely to describe behavior for callers who will
not read the body.

*Avoid:* writing doc comments on private/internal functions that have no
external caller, because no one benefits from API documentation they
cannot call.

## What an internal comment should carry

An internal comment earns its line by carrying information the code
itself does not express. If removing the comment leaves the reader no
worse off, the comment is noise.

- **Non-obvious decisions** — why this approach over the obvious
  alternative. The reader sees the code; they do not see the rejected
  options or the constraint that ruled them out.
- **Edge-case rationale** — why a special case exists. "This branch
  handles cross-version restore" tells the reader what the code alone
  cannot: that the branch exists for a reason that is not local to this
  function.
- **Safety nets** — code that looks unnecessary but is kept as a guard.
  "Logically a no-op; kept so no recorded turn can ever carry an
  unanswered tool_call id" explains why deletion is the wrong instinct.
- **Invariants and constraints** — conditions the code assumes but does
  not enforce, or properties that must hold for correctness. A field
  marked `#[serde(skip)]` with a comment explaining why it is not
  persisted prevents a future maintainer from "fixing" it.
- **Cross-reference to trade-offs** — when a design choice has a known
  cost, naming the cost and the reason it was accepted turns a
  puzzling decision into a documented trade-off.

*Avoid:* narrating what the code does line-by-line, because the code is
authoritative and the narration drifts; use a comment only where the
*why* is not recoverable from reading the code.

*Avoid:* commenting to compensate for unclear code, because the right
fix is clearer code, not a comment explaining the confusion — refactor
first, comment only what remains non-obvious after the refactor.

## Marker annotations: NOTE, TODO, HACK

Markers are scannable annotations — `NOTE:`, `TODO:`, `HACK:` — that
tag a comment with a semantic signal a maintainer can grep for. Each
marker has a contract; using one without honoring the contract produces
a label with no context, which is worse than no marker.

- **`NOTE:`** — a non-obvious constraint or reason the reader must know.
  The most common and safest marker; essentially a why-comment with a
  flag for visibility. Use freely where the reasoning would otherwise be
  missed.

- **`TODO:`** — a discovery outside the current task's scope: an
  improvement you noticed but should not pursue now because it would
  expand scope. Must be specific — name what is missing and suggest the
  direction, not just "TODO: fix this." A vague TODO is a debt with no
  plan; a specific TODO is a recorded observation a future task can pick
  up. The risk is accumulation: TODOs that never get done, so use them
  for genuine scope-boundary discoveries, not for things you intend to
  for genuine scope-boundary discoveries, not for things you intend to
  do in five minutes. A deferred defect is a specific TODO, not a FIXME:
  — record what is broken and the suggested fix direction.

- **`HACK:`** — a workaround for a limitation you cannot currently
  resolve. Must explain both why the workaround is needed and what
  condition would let you remove it (upstream fix, API change, new
  capability). Without the removal condition, a HACK is permanent
  technical debt disguised as temporary.

*Avoid:* `FIXME:` for known problems. If code has a known issue, either
fix it now (through the development workflow) or recognize it as an
acceptable trade-off and document it with `NOTE:` — "FIXME" implies the
code is broken and you are not addressing it, which normalizes leaving
defects in place.

## Decision rules

- If the comment describes what the code does and a reader could infer
  the same by reading the code, then remove it, because it adds length
  without understanding.
- If the function is pub and has no doc comment, then add one, because
  an undocumented API is a contract the caller cannot discover.
- If you are about to write `TODO:`, then state what is missing and the
  suggested direction, because a TODO without specifics is a debt with
  no recovery plan.

## Anti-patterns

- **What-comment** — `// increment i by 1` on `i += 1`, because the code
  is the source of truth and the comment adds nothing a reader could not
  see; it only creates a maintenance burden (change the code, forget the
  comment, now it lies).
- **Stale comment** — code changed but the comment did not, because a
  comment that contradicts the code is worse than no comment: it
  actively misleads the next reader. Comments that describe what (not
  why) go stale most often, because they duplicate the thing most likely
  to change.
- **FIXME as deferred action** — leaving `// FIXME: handle this` and
  moving on, because it turns a known defect into accepted background
  noise; either fix it or document the trade-off.
- **Doc comment on a private function** — `/// Returns the length` on a
  `fn` that is not pub, because no caller needs the documentation and it
  adds noise to the module.
