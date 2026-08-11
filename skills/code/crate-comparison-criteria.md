---
name: Crate Comparison Criteria
description: When you are comparing competing Rust crates for a project need — the Rust-specific dimensions for evaluating ecosystem health, API quality, and project fit that specialize the general dependency-selection criteria
---

# Crate Comparison Criteria — Reference

Rust-specific comparison dimensions for choosing between competing
crates. This skill specializes `code/dependency-selection` (the
cross-language make-or-buy and comparison criteria) to the Rust
ecosystem, adding Rust-specific signals and evaluation techniques. For
where to get accurate data (cargo search/info vs docs.rs vs recall), see
`code/dependency-evaluation`.

## Ecosystem health (Rust-specific)

The general criteria from `code/dependency-selection` apply —
maintenance activity, community adoption, ecosystem integration. In
Rust, these map to concrete signals:

- **crates.io metadata** — `cargo info` surfaces version, license,
  MSRV, and feature flags. Cross-reference with crates.io for download
  counts and reverse-dependency counts (how many other crates depend on
  this one), because a crate that is itself a dependency of many
  projects is more likely to remain maintained.
- **Repository activity** — last commit date, recent commit frequency,
  and open-issue resolution rate. A crate with a stale repository but
  recent crates.io releases may have a slow-but-steady maintainer; a
  crate with neither is likely abandoned.
- **Release cadence** — Rust crates often follow semver strictly. A
  crate that releases minor versions regularly (new features,
  improvements) is actively developed; one that only releases patches
  is in maintenance mode; one with no releases at all may be abandoned.

## Technical quality (Rust-specific)

Rust's type system makes several quality dimensions assessable directly
from source — read the cached source (see `code/rust-crate-evaluation`
for the pipeline and source-reading strategy).

- **Type-level safety** — does the API use Rust's type system to make
  misuse hard? Newtypes, sealed traits, `#[must_use]`, and exhaustive
  enums all push errors to compile time. A crate whose API relies on
  runtime checks where the type system could enforce correctness at
  compile time is lower quality.
- **Error types** — does the crate expose a dedicated error enum with
  meaningful variants, or does it use `Box<dyn Error>` or `anyhow`?
  Specific error types let you handle failures precisely; opaque ones
  force you to match on strings or give up on granular handling.
- **Trait design** — are the traits minimal and focused (e.g.
  `serde::Serialize`), or do they bundle unrelated responsibilities?
  Well-designed traits compose; poorly designed ones force you to
  implement methods you do not need.
- **Unsafe usage** — does the crate use `unsafe`? If so, is it
  justified with safety comments and bounded by a minimal surface?
  Unjustified `unsafe` is a soundness risk; a crate that avoids
  `unsafe` entirely, or confines it to a well-audited core, is safer.
- **Feature flag granularity** — are features fine-grained enough to
  compile only what you need? A crate with no features forces you to
  pull in everything; one with well-named, composable features lets you
  minimize compile time and binary size.

*Avoid:* judging API quality purely from README examples, because
examples show the happy path but not edge cases, error handling, or
type-level safety — read the actual trait and type definitions in the
source.

## Project fit (Rust-specific)

- **Async compatibility** — if your project uses async, does the crate
  support it natively, through a feature flag, or not at all? A crate
  that is sync-only may require `spawn_blocking` workarounds that add
  complexity. Check whether the async runtime (tokio, async-std,
  smol) is configurable or hard-coded.
- **no_std support** — if your target is embedded or WASM, does the
  crate support `no_std`? A crate that requires `std` cannot be used in
  these environments without forking.
- **Philosophy alignment** — Rust crates vary in philosophy: zero-cost
  abstractions, compile-time safety, explicit error handling, minimal
  dependency footprint. A crate whose philosophy matches your project
  integrates smoothly; one that conflicts (e.g. heavy runtime where
  you want zero-cost, panics where you want Results) creates friction.
  An agent can infer this from the crate's `Cargo.toml` dependency list,
  trait design, and error strategy, but weigh this judgment alongside
  concrete API behavior, because philosophical fit is harder to assess
  from code alone than technical correctness.

## Decision rules

- If two crates are comparable on fitness, then prefer the one with
  stronger type-level safety (newtypes, sealed traits, exhaustive
  enums), because compile-time safety reduces runtime bugs and makes
  the API harder to misuse.
- If a crate uses `unsafe`, then check for safety justifications in the
  source, because unjustified `unsafe` is a soundness risk that
  outweighs feature convenience.
- If your project is async or no_std, then verify the crate supports
  your constraint before deeper evaluation, because discovering this
  mismatch late wastes the evaluation effort.
- If a crate's error type is opaque (`Box<dyn Error>`, `anyhow`), then
  weigh whether you need granular error handling, because if you do,
  an opaque error type will force workarounds or a fork.

## Anti-patterns

- **Judging from README only** — evaluating an API from examples rather
  than source, because examples show the happy path but hide error
  handling, type safety, and `unsafe` usage; read the trait and type
  definitions.
- **Ignoring async/no_std constraints** — evaluating a crate fully
  before checking compatibility, because a sync-only crate in an async
  project adds complexity that may make it unfit regardless of other
  qualities.
- **Equating feature count with quality** — preferring the crate that
  does more, because a focused crate with fewer features is often
  better designed and lighter than a kitchen-sink crate; compare on
  fitness for your need, not feature list length.
- **Overlooking error type quality** — treating error handling as an
  afterthought, because the error type determines how robustly you can
  handle failures; a crate with specific error variants is
  significantly easier to integrate correctly.
