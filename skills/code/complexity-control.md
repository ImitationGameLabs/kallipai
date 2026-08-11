---
name: Complexity Control
description: When you are evaluating whether a design or abstraction is over-engineered — the judgment standard for abstraction cost vs benefit, speculative generality, and single-use abstraction signals
---

# Complexity Control — Reference

Over-engineering is the most common design failure: building abstractions and
generality that the problem does not need. This skill is the judgment standard
for distinguishing justified abstraction from premature indirection. It
provides the criteria for the complexity dimension of `code/design-thinking`.
Common sense says "keep it simple"; this skill adds the signals for when
simplicity requires removing an abstraction and when it actually requires one.

## The cost of abstraction

Every abstraction — a function, a trait or interface, a generic parameter, a
layer of indirection — exacts a tax on every future reader: they must follow
the indirection, hold the abstraction's contract in mind, and lose sight of
the concrete behavior behind the interface. The benefit must exceed this tax.

Benefits that justify the tax: genuine reuse (multiple call sites that share
the abstraction), simplification of call sites (the abstraction hides
complexity the caller should not care about), and encapsulation of a volatile
boundary (the abstraction lets the implementation change without breaking
callers).

*Avoid:* abstracting because the code "looks cleaner" or is "more DRY" —
because an abstraction with no concrete benefit is indirection you pay for on
every read forever; inline the code until a benefit appears.

## Single-use abstraction signals

An abstraction used in exactly one place is usually premature, because the
cost (indirection, a new name to learn) is paid without the benefit (reuse).
The signals that a single-use abstraction is premature:

- the abstraction's name is more general than its single use warrants — a
  function called `process_data` called from one place with one argument shape
- the abstraction has parameters or branches that exist for callers that do
  not exist yet
- the call site is no clearer with the abstraction than the inlined code
  would be

*Avoid:* extracting a function or trait on first encounter because "it might
be reused" — because you are paying abstraction tax for a reuse that may
never materialize; wait until the second or third occurrence reveals the real
shared shape, then abstract with confidence. Exception: extract immediately
when the concrete code is complex enough that a named abstraction genuinely
aids readability at the single call site.

## Speculative generality and YAGNI

"You Aren't Gonna Need It" is a heuristic against speculative generality —
building flexibility, configurability, or extensibility for needs that have
not materialized. This is reasoned, not doctrinal: the line is whether the
future need is concrete and imminent or speculative and distant.

- If the future need is speculative and distant ("we might want to support X
  someday"), do not build for it, because the cost is real now and the
  benefit is hypothetical.
- If the future need is concrete and imminent — a clear next requirement, an
  agreed-upon roadmap item — building ahead may be cheaper than refactoring
  later, because the retrofit cost is also real.

*Avoid:* building configurability, plugin systems, or generic frameworks "for
future flexibility" — because flexibility you do not need is complexity you
cannot remove; add the flexibility when the need is concrete, and the design
will be simpler because it solves a real problem instead of an imagined one.

## Decision rules

- If an abstraction has one caller and the call site would be as clear
  without it, inline it, because the indirection tax is paid without reuse
  benefit.
- If an abstraction has two or more callers sharing the same shape, keep it,
  because the reuse benefit exceeds the indirection tax.
- If a function is long but has a single purpose, prefer readability edits
  (extract well-named helpers, reorder for narrative) over architectural
  changes, because complexity from length and complexity from architecture
  are different problems with different remedies.
- If you are tempted to add a parameter, flag, or branch for a scenario that
  does not yet exist, do not add it, because speculative parameters create a
  combinatorial test burden for a need that may never arrive.

## Anti-patterns

- **Premature generalization** — making a function generic or a type
  extensible on first use, because the abstraction tax is paid immediately
  while the reuse benefit is imaginary; prefer concrete code until
  repetition reveals the true abstraction.
- **Configurability theater** — adding configuration options, hooks, or
  extension points for futures that never arrive, because each option adds a
  combinatorial test burden and a maintenance liability for a need that may
  never materialize.
- **Architecture astronautics** — designing layers of abstraction above the
  problem at hand, because each layer adds indirection the reader must
  traverse; the problem's inherent complexity is the floor, and anything
  above it is self-inflicted.
