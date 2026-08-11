---
name: Dependency Selection
description: When you are choosing whether to build or adopt a dependency, and how to compare competing libraries across maintenance health, technical quality, and project fit — the cross-language fallback criteria
---

# Dependency Selection — Reference

The decision to add a dependency is not just "find a library that does
X" — it involves a make-or-buy judgment, a comparison across competing
candidates, and a fitness check against your project. This skill defines
the general criteria that apply across all languages. For Rust-specific
criteria, see `code/crate-comparison-criteria`; for the information-source
hierarchy (where to get accurate data), see `code/dependency-evaluation`.

## Make-or-buy decision

Before searching for candidates, decide whether a third-party dependency
is the right call at all. The default leans toward adopting an existing
library rather than reinventing it, because a mature library has been
tested by real users and embodies domain expertise the agent may lack.
But adoption is not always right.

**Prefer to adopt when:**

- The domain requires specialized expertise you lack — cryptography,
  parsing, concurrency primitives — because a self-implemented version
  is more likely to contain subtle bugs or security flaws than a
  library maintained by domain experts.
- The functionality is well-defined and broadly needed (JSON parsing,
  HTTP client, logging) — because mature, battle-tested options almost
  certainly exist and your version would add nothing.
- The library's API surface is small and stable — because the
  integration cost is low and the ongoing maintenance burden falls on
  the library author, not you.

**Prefer to build when:**

- Existing libraries have poor API design — misuse-prone interfaces,
  leaky abstractions, or excessive coupling — because a dependency you
  must fight is more expensive than a small focused implementation you
  control.
- The needed functionality is small and self-contained enough that the
  dependency's weight (transitive deps, build time, API surface to
  learn) exceeds its benefit, because pulling in a library for a
  20-line utility is net-negative.
- Your requirements are specific enough that no existing library fits
  without significant wrapping or workarounds, because the wrapper
  layer adds complexity without eliminating the dependency risk.

*Avoid:* defaulting to "build it myself" for specialized domains like
cryptography or parsing, because your implementation is unlikely to match
the domain expertise and testing depth of a dedicated library; and
defaulting to "adopt" for trivial, self-contained utilities, because the
dependency overhead is not worth it.

## Comparison dimensions

When evaluating competing candidates, compare across three categories.
These are criteria, not a checklist — weigh them according to your
project's priorities.

### Ecosystem health

Signals about whether a library is alive, maintained, and trusted by
the community. Gathered from the package registry and project
repository metadata.

- **Maintenance activity** — recent commit frequency and last commit
  date indicate whether the project is actively maintained. A library
  with no commits in over a year may be abandoned; one with regular
  releases is likely to fix bugs and track language/ecosystem changes.
- **Community adoption** — download counts and star counts indicate how
  widely the library is used. High adoption suggests the library has
  been exercised in diverse real-world scenarios, which surfaces bugs
  that solo projects miss.
- **Ecosystem integration** — how many other projects depend on this
  library? A library that is itself a dependency of many projects is
  more likely to remain maintained and compatible than an isolated one.

### Technical quality

Signals about the library's design and implementation quality.
Evaluated from source code and API surface.

- **API design** — is the interface hard to misuse? Are types and
  constraints expressive enough to catch errors at compile time (where
  the language supports it)? A well-designed API guides correct usage;
  a poorly designed one requires the caller to remember invariants the
  type system does not enforce.
- **Abstraction quality** — does the library expose clean boundaries
  that do not leak implementation details? Leaky abstractions couple
  your code to the library's internals, making upgrades painful.
- **Error handling** — are errors specific and actionable, or vague and
  opaque? The error model determines how robustly you can handle
  failures.

### Project fit

Signals about alignment with your project's needs and philosophy.

- **Philosophy alignment** — does the library's design philosophy
  (e.g. zero-cost abstractions, minimal runtime, explicit error
  handling) match your project's? A mismatch creates friction at every
  integration point. Note: an agent can infer philosophy from README,
  source structure, and API conventions, but this judgment is less
  reliable than a human's — weigh it but verify against concrete API
  behavior.
- **Feature match** — does it provide the specific features you need
  without excessive surface area you do not? A library that does
  everything is harder to learn and integrate than one focused on your
  use case.

*Avoid:* comparing only on download count or star count, because
popularity does not guarantee fitness — a popular library with a poor
API may cost more to integrate than a smaller, well-designed alternative.

## Decision rules

- If the domain is specialized (crypto, parsing, concurrency), then
  adopt rather than build, because the risk of subtle bugs in a
  self-implementation outweighs the dependency cost.
- If candidates exist but all have poor API design, then weigh building
  a focused implementation, because a dependency you must fight is more
  expensive than code you control — but only if the functionality is
  small enough to implement correctly.
- If two candidates are comparable on fitness, then prefer the one with
  stronger ecosystem health (more active maintenance, wider adoption),
  because it is more likely to remain compatible and receive bug fixes.
- If no candidate clearly fits, then broaden the search before defaulting
  to build, because a niche library you missed may solve the problem
  better than a fresh implementation.

## Anti-patterns

- **Building for specialized domains** — implementing your own crypto
  or parser, because the domain expertise and testing depth of a
  dedicated library is very hard to replicate; prefer adoption here.
- **Comparing only on popularity** — picking the most-downloaded
  library without reading its API, because popularity reflects
  historical visibility, not current fitness or design quality.
- **Adopting a heavy library for a small need** — pulling in a
  framework to use one function, because the dependency weight and
  learning cost exceed the benefit; a small self-implementation may be
  simpler.
- **Ignoring maintenance signals** — adopting a library without
  checking last release date, because an unmaintained dependency
  becomes a security and compatibility liability.
