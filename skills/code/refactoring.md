---
name: Refactoring
description: When you are improving the structure of working code without changing its behavior — the test-verified small-step discipline that keeps refactorings safe
---

# Refactoring — the behavior-preserving change workflow

This is the ordered workflow for restructuring working code to improve its
clarity, simplicity, or extensibility without changing what it does. The
discipline is a safety net of tests verified before and after each small
change, because without that net you cannot know whether behavior was
preserved.

The defining constraint of refactoring is **behavior preservation**: the
code's observable behavior must be identical before and after. If behavior
needs to change, that is `code/development`, not refactoring — even if the
change includes structural cleanup.

## When to use

- Code works but is hard to understand, modify, or extend, and you want to improve its structure without changing what it does
- You need to make a future change but the current structure makes it painful, so you restructure first
- Dead code, duplication, or tangled responsibilities are adding maintenance cost

## When NOT to use

- When behavior must change — use `code/development`, because refactoring preserves behavior by definition; a change that alters behavior needs development's plan-review for the new behavior's correctness
- When the code has no tests and you cannot write them — first write characterization tests (via `code/development` if needed), because refactoring without a safety net is gambling on behavior preservation
- For a one-line rename or trivial cleanup — act directly and run tests, because the full sequence is ceremony for a change that carries no structural risk
- When the root cause of a bug is unknown — start with `code/debugging`, because the investigation must precede any restructuring

## The sequence

**Establish the safety net.** Before moving anything, confirm that the code
has tests that exercise its observable behavior, and that they pass. The
tests are your oracle: they tell you whether each step preserves behavior.
If there are no tests — or the tests cover too little of the behavior you
plan to touch — write characterization tests first. These are tests that
capture the code's *current* behavior (not its ideal behavior), because the
goal is to detect change, not to assert correctness of the existing logic.

Done when:

- the test suite passes on the current code
- the tests exercise the behavior of the code you plan to restructure (not just peripheral paths)

**Understand the current structure.** Read the code thoroughly. Identify
the structural problem you are solving — is it tangled responsibilities
(one function doing three things), duplication, poor names, dead code,
excessive coupling, or missing abstraction? Load `code/complexity-control`
to evaluate whether an abstraction is justified or whether the code needs
*less* indirection, not more.

Done when:

- you can name the specific structural problem in one sentence
- you can state why it matters (what future change does it block or make expensive?)

**Make one small change.** Apply a single refactoring move — extract a
function, inline a function, rename, move code between modules, remove dead
code. Each move is atomic: it changes structure without changing behavior,
and it is small enough that if tests fail you can identify exactly what
broke. Load `code/testing` to confirm the tests you rely on are testing
behavior (not implementation), because implementation-coupled tests will
fail on a correct refactoring and give false signals.

Done when:

- one structural change is applied to the code
- the change is small enough to describe in one sentence

**Verify.** Run the full test suite. If tests pass, behavior is preserved
and the refactoring is safe to keep. If tests fail, revert the change (or
fix it if the failure reveals a mistake in the move, not a behavior change),
because a test failure during refactoring means either the move was wrong
or the test is coupled to implementation — both must be resolved before
continuing.

Done when:

- the full test suite passes after the change

**Commit.** After each verified change, commit. Each commit is a
checkpoint: if a later step reveals a subtle problem, you can revert to the
last known-good state rather than undoing a large batch of changes. Follow
`code/committing` for the workflow and `code/commit-messages` for the
message standard — a refactoring commit should describe what structural
change was made and why.

Done when:

- the verified change is committed with a message describing the refactoring

**Repeat or stop.** Return to "Make one small change" for the next move
toward your structural goal. Stop when the structural problem is solved —
the code is clear enough to work with, the duplication is removed, the
responsibilities are untangled. Do not refactor beyond the goal, because
each additional change carries risk without targeted benefit, and "cleaner"
is subjective once the concrete problem is solved. For larger restructuring
that involves design decisions (new abstractions, architectural changes),
load `code/design-thinking` to evaluate approaches before proceeding.

Done when:

- the named structural problem is resolved
- the test suite still passes

## Key behaviors to remember

- **Tests before structure.** The safety net must exist before the first change, because without it you cannot distinguish a behavior-preserving refactoring from a silent behavior change.
- **One move at a time.** Each step is a single, atomic refactoring verified by tests, because batching changes makes it impossible to identify which step introduced a behavior change if tests fail.
- **Commit after each verified step.** Each commit is a rollback checkpoint, because the finest-grained undo history is a sequence of small commits, each verified green.
- **Behavior preservation is the invariant.** If the refactoring changes observable behavior, it has stopped being refactoring, because the goal is structure change only — if behavior must change, switch to `code/development`.

## Anti-patterns

- **Rewriting instead of refactoring** — replacing a large body of code wholesale and checking tests at the end, because a wholesale rewrite conflates structural change with behavior change and you cannot isolate which step altered behavior if tests fail; refactor in small verified steps.
- **Refactoring without tests** — restructuring code with no test suite to verify behavior, because you are asserting behavior preservation without evidence; write characterization tests first.
- **Big-batch commits** — accumulating multiple refactoring moves before committing, because the batch gives you no rollback granularity; commit after each verified step.
- **Refactoring beyond the goal** — continuing to restructure once the concrete problem is solved, because each change beyond the goal carries risk without targeted benefit and "cleaner" is subjective; stop when the named problem is resolved
- **Trusting implementation-coupled tests** — relying on tests that assert internal call sequences or private state, because they will fail on a correct refactoring and give false signals; the safety net must test behavior through the public interface.
