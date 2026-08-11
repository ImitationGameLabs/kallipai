---
name: Debugging
description: When code is failing — a test failure, a runtime error, or unexpected behavior — and you need to find and fix the root cause through systematic investigation
---

# Debugging — the root-cause investigation workflow

This is the ordered workflow for finding and fixing a bug. Each step narrows
the search: reproduce the failure, extract what the error tells you, isolate
the failing code, prove a root-cause hypothesis, then fix and guard it. The
discipline is investigation before action — verify the cause before changing
code, because an unverified fix is a guess that may not address the real
problem.

This skill covers the investigation and the regression guard. If the fix is
non-trivial — a design change, not a one-line correction — transition to
`code/development` for the plan-implement-review workflow once the root cause
is confirmed.

## When to use

- A test fails and the cause is not immediately obvious from the assertion message
- Code produces a runtime error — a panic, exception, or crash — and you need to find why
- Behavior is incorrect or unexpected and you must trace the cause

## When NOT to use

- For a compiler error the message already pinpoints — read it and fix the syntax or type, because the investigation overhead costs more than the fix
- For a one-line fix where the cause is obvious from the error — fix directly and add a regression test, because the full investigation sequence is ceremony
- To explore a codebase without a specific failure — use `code/exploring`
- To restructure working code without fixing a bug — use `code/refactoring`, because debugging is investigation of a failure, and behavior-preserving restructuring needs its own safety-net workflow

## The sequence

**Reproduce.** Get a reliable, repeatable reproduction before anything else.
Without reproduction you cannot verify the fix, because a bug you cannot
trigger is a bug you cannot confirm is gone. Note the exact steps, inputs, and
environment that trigger it. Minimize the reproduction — the smallest case
that triggers the failure is the easiest to reason about.
Done when:

- the failure can be triggered on demand with known inputs
- the reproduction is as minimal as you can make it

**Read the error.** Before forming any hypothesis, extract everything the
failure tells you: the error message, the stack trace, the assertion that
failed, and the last log lines before the crash. The error is primary data —
it often points directly at the file, function, and line where the logic
broke. Read the full trace, not just the top line, because the root cause is
usually deeper in the call stack than the symptom.
Done when:

- you can state in one sentence what the error says and where it points
- you have read the full stack trace or assertion, not just the headline

**Isolate.** Narrow the failure to the smallest scope that still reproduces.
Bisect: if a test suite fails, run individual tests; if a test fails,
identify the failing assertion; if a function produces wrong output, identify
which input or branch triggers it. `git bisect` can find the introducing
commit; commenting out code paths can isolate the failing branch. The goal is
to reduce the search space from "the whole program" to "this function, this
branch, this condition."
Done when:

- the failure is traced to a specific function and code path
- you can state the boundary: everything before X works; at X, the output is wrong

**Form a hypothesis.** Based on the isolated scope and the error data, state a
specific, falsifiable hypothesis about the root cause — "X returns the wrong
value because Y, which causes Z." Read the code at the isolated point to
understand the logic before theorizing. A good hypothesis explains both the
failure and the original assumption that no longer holds.
Done when:

- the hypothesis predicts an observable value or behavior (it is testable)
- it explains the full chain from root cause to observed symptom

**Test the hypothesis.** Prove or disprove it with the cheapest possible probe
before changing code. An `assert!` or debug print at the suspected point
confirms whether the value matches the prediction; a focused test isolates the
behavior; a temporary log reveals the runtime state. If the probe disproves
the hypothesis, form a new one at the same scope — the isolation holds,
only the theory was wrong. Return to Isolate only if the probe shows the
boundary itself is wrong. Probe one variable at a time, because multiple
simultaneous changes obscure which one mattered.
Done when:

- the probe confirms or refutes the hypothesis with observable evidence
- for a confirmed hypothesis: the root cause is identified and explained

**Fix the root cause.** Correct the underlying error, not the symptom. The
root cause is the reason the bug exists; the symptom is what you observed. A
symptom-level fix — catching the error, patching the output — moves the bug or
hides it, because the underlying logic is still wrong. For a trivial fix (a
wrong operator, a missing condition), apply it directly. For a non-trivial fix
(a design flaw, a missing abstraction), transition to `code/development` to
plan the fix properly.
Done when:

- the fix addresses the root cause identified in the hypothesis
- the reproduction case now passes

**Guard the fix.** Add a regression test that captures the original failure
mode — the exact scenario that was broken. The test documents the fix and
prevents reintroduction. Follow `code/testing` for test quality (behavior not
implementation, regression value, naming). Run the full test suite to confirm
the fix introduced no new failures.
Done when:

- a regression test exists that would fail without the fix
- the full test suite passes

## Key behaviors to remember

- **Reproduce before debugging.** A bug you cannot reproduce is one you cannot verify the fix for, because without a reliable trigger you cannot distinguish "fixed" from "did not fire this time."
- **Read the full error before hypothesizing.** The stack trace and assertion message are the most direct evidence available, because they are the system's own account of what went wrong and where.
- **One hypothesis at a time.** Change one thing, probe, observe — because multiple simultaneous changes obscure which one mattered and leave the root cause ambiguous.
- **Verify the cause before fixing.** A plausible-sounding cause that has not been probed is a guess, because pattern-matching to a familiar bug shape feels productive but skips the evidence that confirms it.
- **Fix the root cause, not the symptom.** A symptom patch hides the bug rather than killing it, because the underlying logic is still wrong and the symptom will resurface in a different form.

## Anti-patterns

- **Shotgun debugging** — changing multiple things at once hoping one works, because you cannot attribute the fix to any single change and the root cause remains unknown; probe one hypothesis at a time.
- **Fixing symptoms** — catching the error or patching the output without understanding why, because the underlying logic is still wrong and the bug resurfaces; trace to the root cause before changing code.
- **Skipping reproduction** — "fixing" a bug you cannot trigger, because you have no way to confirm the fix worked; establish a reliable reproduction first.
- **No regression test** — shipping a fix without a test that captures the failure mode, because the fix is unguarded and the bug can be reintroduced silently; add a test that fails without the fix.
- **Hypothesizing before reading the error** — theorizing about the cause before extracting what the error says, because the error often points directly at the problem and a premature hypothesis biases your reading toward confirmation.
