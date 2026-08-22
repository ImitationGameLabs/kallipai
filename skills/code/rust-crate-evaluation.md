---
name: Rust Crate Evaluation
description: When you are about to add a Rust crate — source-reading strategy for Rust's expressive type system, and version, dependency, and feature judgment specific to the Rust ecosystem
---

# Rust Crate Evaluation — evaluate before you depend

Rust's tooling makes crate evaluation efficient for an agent: `cargo`
provides real-time metadata and caches source locally, and Rust's type
signatures are often self-documenting. This skill specializes
`code/dependency-evaluation` (the cross-language information-source
hierarchy) to the Rust ecosystem. For the general principles, see
`code/dependency-evaluation`; this skill covers the cargo-specific pipeline
and Rust-specific source-reading strategy.

## When to use

- You are about to add a crate to `Cargo.toml` and need to choose or
  verify it
- You need to understand a crate's API contract before using it

## When NOT to use

- To look up the general information-source hierarchy (package manager vs
  web vs recall), because that is `code/dependency-evaluation`; this skill
  covers the Rust-specific specialization.

## The pipeline

**Search for candidates.** Run `cargo search <keyword>` to discover
crates matching the need. The output is real-time: current version
numbers and one-line descriptions, sorted by relevance. Treat the
results as a candidate list, not a decision — the top result is not
necessarily the best fit.
Done when:

- you have 2–5 candidates with their current version numbers

**Inspect metadata.** Run `cargo info <crate>` on each candidate. This
fetches real-time metadata and downloads the crate source to the local
cargo registry. The output surfaces: latest version (and whether the
queried version differs), license, MSRV (`rust-version`), features with
their dependency graph, and repository link.
Done when:

- you have the metadata for each candidate
- the crate source is cached locally (confirmed by checking the registry
  path — see "Locating cached source" below)

**Read the source.** Open the cached source to verify the API contract
and confirm the crate actually provides what you need. This is the
decisive step — metadata tells you *about* the crate; source tells you
*how to use it* and *whether it fits*. Rust's type signatures make this
efficient (see "Source-reading strategy" below).
Done when:

- you have read the `lib.rs` public API and the relevant trait/error
  definitions
- you can state the key types, function signatures, and error types the
  crate exposes
- you have confirmed the feature you need exists in the source, not just
  the README

## Locating cached source

`cargo info` downloads and unpacks the crate source to the local
registry:

```bash
# The path has a registry hash component — glob it rather than hardcoding
ls ~/.cargo/registry/src/*/<crate>-<version>/src/lib.rs
```

The registry directory name (e.g. `index.crates.io-1949cf8c6b5b557f`)
is a hash that varies by registry mirror, so always glob with `*`. The
version matches what `cargo info` reported — use it to pinpoint the
exact directory when multiple versions are cached.

*Avoid:* hardcoding the registry hash, because it changes between
machines and registry mirrors; glob with `~/.cargo/registry/src/*/`.

## Source-reading strategy

Rust's type system is documentation. Read source in this order to
maximize information per token:

1. **`src/lib.rs`** — the crate root. Its `pub` items are the public
   API surface; everything `pub use`-d is the intended interface. This
   is the crate's table of contents.
2. **Trait definitions** — search for `trait` to find the abstractions
   the crate models. Trait bounds tell you what capabilities a type must
   provide; method signatures tell you the contract.
3. **Error types** — find the error enum or struct (often `error.rs` or
   in `lib.rs`). Is it a custom `enum` with variant-per-error, or
   `anyhow::Error`? This determines how you handle failures.
4. **`examples/` directory** — if present, these are end-to-end usage
   demonstrations written by the crate author. They show the intended
   calling pattern, not just the type signature.
5. **Implementation** — read the actual function bodies only when you
   need to understand runtime behavior, edge cases, or performance
   characteristics that the signature does not convey.

*Avoid:* reading every source file, because most of the value is in the
`pub` interface layer; implementation detail consumes context tokens
without changing your API-level decisions unless you suspect a
behavioral issue.

## Version and maintenance judgment

- **`cargo info` shows `version: X (latest Y)`** — if X ≠ Y, the
  version you asked about is not the latest. This is a signal to check
  the changelog or decide whether to pin X or upgrade to Y.
- **0.x versions** mean the author does not promise API stability; a
  0.3 → 0.4 bump can be a breaking change. Evaluate whether the crate
  is mature enough for your use case.
- **MSRV (`rust-version`)** must be compatible with your project's Rust
  toolchain; a crate requiring a newer Rust than your project will not
  build.
- **Last publication date** is visible from the registry — a crate with
  no releases in years may be abandoned, even if it works today.

## Dependency and feature judgment

- **Dependency weight**: `cargo info` does not show dependencies in
  its default output (only under `-v`); use `cargo tree` after adding the
  crate to see the full direct and transitive tree. A crate that pulls in 50
  transitive dependencies for one feature may not be worth it — look for a
  lighter alternative or use feature flags to disable unneeded code paths.
- **Feature flags**: `cargo info` lists features and what they enable.
  Many crates support a minimal default; enable only the features you
  need to reduce compile time and binary size. Check whether the feature
  you need is behind a non-default flag.

*Avoid:* adding a crate with default features without checking what they
enable, because defaults often pull in optional dependencies (TLS,
async runtimes, etc.) you may not need; specify `default-features =
false` and enable only what you use.

## Decision rules

- If the crate's source is cached locally after `cargo info`, then read
  it instead of visiting docs.rs, because the source is the ground truth
  that docs.rs renders from — reading it directly avoids HTML noise and
  rendering lag (docs.rs may not have built the latest version yet).
- If two crates serve the same need, then compare their dependency
  weight and feature granularity, because a lighter crate with fewer
  transitive dependencies is preferable when fitness is comparable.
- If the crate is pre-1.0, then weigh the churn risk against the
  alternatives, because a 0.x crate can break your build on a minor
  version bump.

## Anti-patterns

- **Recall-driven version pinning** — writing `foo = "1.2"` from memory,
  because the current version may be 2.0 with a different API; always
  `cargo search` or `cargo info` to get the real version.
- **docs.rs instead of source** — browsing rendered HTML when the source
  is cached locally, because docs.rs adds HTML noise and may lag behind
  the latest release; the source is already on disk after `cargo info`.
- **Reading README only** — trusting the description without reading
  source, because README claims and actual implementation can diverge;
  verify the API contract in `lib.rs`.
- **Ignoring feature flags** — accepting all default features, because
  defaults may enable heavy optional dependencies; check `cargo info`
  output and disable what you do not need.
- **No dependency-weight check** — adding a crate without checking its
  transitive dependency count, because one crate can pull in dozens of
  others; check `cargo tree` after adding.
