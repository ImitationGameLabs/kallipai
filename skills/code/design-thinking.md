---
name: Design Thinking
description: When you are in the design phase of a change and need to explore and evaluate approaches before converging — divergent analysis across pattern fit, complexity, coupling, and testability
---

# Design Thinking — the design-phase thinking process

This is the thinking process for the design phase of a change — generating
candidate approaches, evaluating them along design dimensions, and converging
on one. It deepens the Plan step in `code/development`; the output standard
for how to present the recommended approach is the Design plan shape in
`what-makes-a-good-plan`.

## When to use

- You are in the Plan step of a change and the design is non-trivial — the
  right approach is not obvious from the problem statement
- You need to evaluate competing design approaches against each other before
  committing to one

## When NOT to use

- For a trivial change where there is only one obvious approach — proceed
  directly, because divergent analysis costs more than it saves
- To produce the plan document itself — that is `planning`; this skill is the
  thinking that fills in the design section of that plan
- To judge plan quality — that is `what-makes-a-good-plan`; this skill is how
  you arrive at the content
- For a pure refactoring with no design decisions — use `code/refactoring`, because small structural moves with no new abstractions need the safety-net workflow, not divergent design analysis

## The sequence

**Pattern match.** Identify what kind of problem this is and match it to
proven structures. Is this a state machine, a parser, a pipeline, a visitor,
a data transformation? Recognizing the pattern gives you a vocabulary and a
proven structure to start from. Check the codebase for existing patterns and
conventions — the exploration in `code/exploring` or `code/onboarding`
should surface these.
Done when:
- the problem type is named (e.g., "this is a state machine with three states")
- applicable patterns from the codebase or standard practice are identified

**Diverge.** Generate 2-3 candidate approaches using the matched patterns as
starting points. Aim for genuinely distinct approaches — different
structures, different decomposition boundaries — not minor variations of one
idea. Sketch the shape of each (modules, data flow, key interfaces), not
line-level code.
Done when:
- 2-3 distinct approaches are sketched at the level of structure, not implementation
- each approach is different enough that rejecting one does not reject all

**Assess each approach.** Evaluate every candidate along three independent
design dimensions:
- *Complexity* — does the approach add indirection beyond what the problem
  needs? Consult `code/complexity-control` for the over-engineering judgment
  standard.
- *Coupling* — how does the approach interact with existing modules? Does it
  create new dependencies, require wide-reaching changes, or introduce
  circular dependencies? An approach that touches fewer files and creates
  fewer new dependencies is usually better, all else equal.
- *Testability* — will code following this approach be testable through its
  public interface? Can the behavior be verified independently? Consult
  `code/testing` for the test-quality standard; a design that forces tests to
  reach into internals is a design smell.
Done when:
- each approach is evaluated on all three dimensions
- at least one dimension distinguishes the approaches (if all three are
  identical, the approaches are not distinct enough — go back and diverge more)

**Converge.** Select the approach with the best tradeoff profile across the
three dimensions. The simplest approach that satisfies coupling and
testability constraints is usually the winner, but not always — a slightly
more complex approach may be justified by materially better coupling or
testability. Present the selected approach as the recommended option in the
plan per `what-makes-a-good-plan`'s Design plan shape.
Done when:
- one approach is selected with its tradeoffs stated
- each rejected approach has a one-line reason it lost
- the selected approach is ready to become the design section of the plan

## Key behaviors to remember

- **Pattern match before diverging** — knowing the problem type constrains
  the search space, because candidate approaches generated without pattern
  awareness are usually unconscious variations of the same idea.
- **Diverge before converging** — generate alternatives before judging them,
  because the first approach that comes to mind is often the most
  conventional, not the best; the evaluation step needs real alternatives to
  compare.
- **The three dimensions are independent** — a simple approach may have bad
  coupling, a well-coupled approach may be hard to test, and a testable
  approach may be over-engineered, because each dimension exposes problems
  the others hide.

## Anti-patterns

- **Designing from scratch when a pattern fits** — reinventing a proven
  structure because you skipped the pattern match, because the codebase or
  standard practice already solved the structural problem.
- **Single-approach design** — converging on the first idea without
  diverging, because the first idea is rarely the best and you have no basis
  for comparison.
- **Evaluating only complexity** — judging approaches on simplicity alone,
  because coupling and testability often expose fatal flaws that simplicity
  analysis misses.
- **Line-level design** — sketching implementation detail during the
  divergent phase, because design thinking is about approach shapes and
  tradeoffs, not code-level decisions; those come during implementation.
