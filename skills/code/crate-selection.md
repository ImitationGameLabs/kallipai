---
name: Crate Selection
description: When you need a Rust crate for a feature and must search, compare, decide make-or-buy, and select — the ordered selection workflow from discovering candidates through committing to a choice
---

# Crate Selection — choose the right crate

The process of going from "I need capability X" to "this is the crate"
(or "I will build it myself"). It delegates comparison criteria to
`code/crate-comparison-criteria` (Rust-specific) and
`code/dependency-selection` (general make-or-buy), and hands off to
`code/rust-crate-evaluation` for the post-selection verification
pipeline. This skill owns only the ordered selection sequence.

## When to use

- You are starting work on a feature and need a Rust crate to provide a
  capability you do not already have
- You are deciding whether to adopt a crate or implement the
  functionality yourself

## When NOT to use

- To look up comparison dimensions (ecosystem health, API quality), because
  that is `code/crate-comparison-criteria`; this skill only runs the
  sequence.
- To verify a crate you have already chosen, because that is
  `code/rust-crate-evaluation`; this skill ends when the choice is made.
- To look up the information-source hierarchy, because that is
  `code/dependency-evaluation`.

## The sequence

**Frame the need.** State precisely what capability you need and what
constraints apply (async, no_std, MSRV, performance, license). A vague
need ("JSON handling") produces too many candidates; a precise one
("streaming JSON parser, no_std compatible, zero-copy") narrows the
field before you search.
Done when:
- the capability is stated as a one-sentence requirement
- the hard constraints are listed (async/no_std/MSRV/license as
  applicable)

**Make-or-buy decision.** Apply the criteria in
`code/dependency-selection` to decide whether to search for a crate or
build it yourself. The default leans toward adoption — especially for
specialized domains like cryptography — but building is justified when
existing options are poorly designed or the need is small enough that a
dependency's weight exceeds its benefit.
Done when:
- you have a reasoned go-search or go-build decision with reference to
  the make-or-buy criteria in `code/dependency-selection`

**Search for candidates.** Run `cargo search <keyword>` to discover
candidates matching the need. Treat the results as a candidate list, not
a ranking — the top result reflects search relevance, not fitness.
Cross-reference with crates.io for download counts and
reverse-dependency counts to gauge ecosystem adoption. Aim for 2–5
candidates; if fewer, broaden the keywords or check the dependency lists
of similar crates for suggestions.
Done when:
- you have 2–5 candidates with their current version numbers and basic
  ecosystem metadata (downloads, last release date)

**Compare candidates.** Apply the Rust-specific comparison dimensions
from `code/crate-comparison-criteria`: ecosystem health, technical
quality (read the source — see `code/rust-crate-evaluation` for the
source-reading strategy), and project fit. Filter first on hard
constraints (async, no_std, MSRV) to eliminate non-starters, then rank
the survivors on quality and fit.
Done when:
- each candidate is evaluated against the comparison dimensions
- hard-constraint failures are eliminated
- the survivors are ranked with a rationale for the ordering

**Select.** Choose the top-ranked surviving candidate. If no candidate
clearly wins, either broaden the search (go back to "Search for
candidates") or, if the need is small enough, revisit the make-or-buy
decision. Do not select a crate you cannot justify over its
alternatives — a forced choice is a signal to search more or build.
Done when:
- one crate is selected with a stated rationale comparing it to the
  alternatives considered
- or, the decision to build is made with a stated rationale

**Verify the choice.** Hand off to `code/rust-crate-evaluation` for the
post-selection verification pipeline: `cargo info`, read the cached
source, confirm the API contract, check dependency weight and feature
flags. Selection and verification are separate because a crate that
looks right on paper can fail on source inspection — type safety gaps,
opaque error types, or `unsafe` without justification.
Done when:
- the selected crate has passed the verification pipeline in
  `code/rust-crate-evaluation`, or the verification surfaces a problem
  that sends you back to "Compare candidates"

## Key behaviors to remember

- **Frame before searching** — a precise need statement with hard
  constraints filters candidates before you spend evaluation effort,
  because a vague need produces too many candidates to compare
  thoroughly.
- **Filter on hard constraints first** — eliminate candidates that fail
  async/no_std/MSRV requirements before reading their source, because
  reading the source of a crate you cannot use is wasted effort.
- **Selection precedes verification** — compare on observable signals
  (metadata, API surface, ecosystem health) to choose, then verify the
  choice by reading source in depth, because deep verification of every
  candidate is too expensive; select first, verify the winner.
- **A forced choice is a signal** — if no candidate clearly wins, do not
  pick the least-bad option by default, because a marginal crate may
  cost more than building the feature; broaden the search or reconsider
  make-or-buy.

## Anti-patterns

- **Searching with vague keywords** — running `cargo search "json"`
  without constraints, because the result set is too large and
  undifferentiated to evaluate; frame the need first.
- **Evaluating every candidate in depth** — reading the full source of
  each of 5 candidates, because deep evaluation is expensive in context
  tokens; filter on metadata and hard constraints, then deep-read only
  the survivors.
- **Selecting without comparing** — picking the first search result,
  because search relevance does not correlate with fitness; compare at
  least 2–3 candidates against the criteria.
- **Skipping verification after selection** — adding the crate to
  `Cargo.toml` immediately after choosing it, because selection
  evaluates on observable signals but the source may reveal problems
  (type safety, unsafe, error quality) that change the decision; always
  run the verification pipeline.
