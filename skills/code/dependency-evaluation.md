---
name: Dependency Evaluation
description: When you are about to add a third-party dependency to a project — the information-source hierarchy for choosing and verifying a library, preferring package managers and structured data over LLM memory or noisy web pages
---

# Dependency Evaluation — Reference

Adding a dependency is a commitment: it enters the build, increases
attack surface, and can break on upgrade. The agent's job is to choose
wisely using the freshest, most accurate information available — not the
possibly-stale knowledge baked into training data. This skill defines the
information-source hierarchy and decision criteria that apply across all
languages. For Rust-specific evaluation via `cargo`, see
`code/rust-crate-evaluation`.

## The information-source hierarchy

An agent evaluating a dependency faces several information sources. Their
reliability is not equal:

1. **Package manager output** (real-time, structured) — `npm info`, `pip
   index`, `go list -m -versions`, `cargo info`, etc. This is the
   freshest source: current version, publication date, download counts,
   dependency tree, license. Prefer this over recall, because it is
   real-time while your training data has a fixed cutoff.
2. **Project source code / repository** (ground truth) — the actual
   implementation. Interfaces, behavior, and edge cases live here, not
   in secondary descriptions.
3. **Project website / documentation pages** (real-time, but noisy) —
   when the package manager does not surface enough detail (e.g.
   migration guides, design rationale), access the project's website.
   The exact tool does not matter — any web access works — but expect
   HTML noise and prefer source code when available.
4. **LLM training data** (convenient, but stale) — a useful starting
   point for recall and hypothesis, but never a substitute for
   real-time verification, because version numbers, API signatures, and
   even whether a package is maintained can have shifted since training.

*Avoid:* trusting your training-data recall for version numbers or API
signatures without verification, because packages release on their own
schedule and your knowledge may be months or years stale — verify with
the package manager first.

## Two principles

- **Real-time tools outrank training memory.** A package's current
  version, maintenance status, and dependency footprint are facts that
  change without your knowledge; always confirm them via the package
  manager before committing to a dependency, because stale information
  leads to wrong versions, deprecated APIs, or abandoned packages.
- **Source outranks derived documentation.** API docs are generated from
  source code; reading the source directly avoids the rendering layer's
  noise and distortion. When the source is available (and for compiled
  languages with expressive type systems it is often self-documenting),
  prefer it over rendered documentation pages.

## Decision criteria

Regardless of language, evaluate a candidate dependency against:

- **Fitness** — does it actually provide the capability you need? Verify
  in source or docs, not just the description, because README claims and
  real behavior can diverge.
- **Maintenance** — when was the last release? Is it actively maintained?
  A dependency with no releases in years is a liability,
  because security and compatibility issues will go unaddressed.
- **Dependency weight** — how many transitive dependencies does it pull
  in? A lightweight wrapper that drags in hundreds of packages may not be
  worth it.
- **Version maturity** — prefer stable releases (1.0+) over pre-1.0
  packages whose APIs may break without notice, unless the pre-1.0
  package is clearly the best option and you accept the churn.
- **License compatibility** — does the license permit use in your
  project?

## When to access the web

When the package manager does not provide enough information to decide —
e.g. you need a migration guide, a design rationale, or a comparison the
package metadata cannot surface — access the project's website. The
choice of tool (web fetch, web search, or any future capability) is
deliberately left open; what matters is that you seek the information
from its primary source rather than relying on stale recall.

*Avoid:* defaulting to web access before trying the package manager,
because package-manager output is structured and low-noise compared to
HTML pages; exhaust the package manager first.

## Decision rules

- If the language has a package manager that provides metadata and
  source, then use it as the primary evaluation channel, because it is
  real-time and structured; this is what `code/rust-crate-evaluation`
  specializes.
- If the package manager does not surface enough detail to decide, then
  access the project website for the missing information, because the
  package manager covers metadata but not always rationale or guides.
- If you are recalling a package from training data, then treat it as a
  hypothesis to verify, not a fact, because the package may have changed
  version, API, or maintenance status since your training cutoff.

## Anti-patterns

- **Trusting recall for versions** — adding `foo = "1.2"` from memory
  without checking, because the latest may be 3.0 with breaking changes
  or the package may be abandoned; verify with the package manager.
- **Skipping maintenance check** — adding a dependency without checking
  last release date, because an unmaintained dependency becomes a
  security and compatibility liability.
- **Web-first instead of package-manager-first** — going to a website
  before trying the package manager, because structured CLI output is
  faster to parse and lower-noise than HTML; exhaust the package manager.
- **Evaluating only the top result** — picking the first search hit
  without comparing alternatives, because fitness, maintenance, and
  dependency weight vary significantly across packages that serve the
  same need.
