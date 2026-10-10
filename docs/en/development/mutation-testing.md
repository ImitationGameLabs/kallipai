---
title: Mutation testing
description: Running cargo-mutants against the Rust workspace on demand.
order: 50
internal: true
---

Mutation testing asks a different question from the test suite: not "do the
tests pass" but "do the tests fail when the behavior changes". `cargo-mutants`
edits one expression at a time (a mutation), runs the tests, and reports whether
the suite noticed. It is an on-demand review tool -- not part of the gate suite
and not wired into CI -- because a run costs minutes and its output is a list of
questions for a human, not a pass/fail verdict.

## Invocation

`cargo-mutants` ships in the backend devShell (`nix/devshells/default.nix`); run
it from the repository root inside `nix develop`. A bounded run looks like:

```sh
CARGO_BUILD_WARNINGS=allow cargo mutants --in-place \
  --package kallipai-adk \
  --file crates/kallipai-adk/src/retry/decision.rs \
  --all-features --test-package kallipai-adk
```

Each flag carries a decision:

- `--package` and `--file` narrow the run to one crate and one file; a
  whole-workspace run is not bounded and is not the intended shape.
- `--test-package kallipai-adk` runs the owning package's tests. Without it,
  cargo-mutants runs every workspace test target for each mutant.
- `--all-features` matches the feature set the gate suite is verified under.
- `--in-place` makes cargo-mutants rewrite the source file in the working tree
  and restore it after each mutant, instead of copying the tree to a scratch
  directory first. The default copy mode also copies `target/` (over a terabyte
  here), so it is not viable.
- `CARGO_BUILD_WARNINGS=allow` matters because the repository's
  `.cargo/config.toml` sets `[build] warnings = "deny"`. Under that setting a
  mutant that trips an unused-variable lint fails to build and cargo-mutants
  reports it as `unviable` instead of testing it. `--cap-lints true` does not
  override this -- it caps rustc's lints, not cargo's `build.warnings`
  configuration -- so the environment variable is the working form.

While a run is in progress the working-tree file is being rewritten. Do not run
git commands against that file until the run finishes, and check afterwards that
`git status` shows it unchanged: cargo-mutants restores the file after each
mutant and after an interrupt, but the check is cheap.

## Scope and cost

Start from a file or a module, not the workspace. `--list` prints the mutants
without running them, which is the way to size a run before committing minutes
to it. `--shard k/n` splits one run across shells, `--re` and `--exclude-re`
filter by mutant name, `-o` sets the output directory, `--iterate` reuses
earlier results, and defaults can live in `.cargo/mutants.toml`.

Measured on this repository (48 cores, warm `target/`) for the file above: 91
mutants, an unmutated baseline of a few seconds, about 6s per viable mutant (and
under a second for each unviable one, which fails the build), and 8 minutes
wall clock in total. A cold `target/` adds the crate's build time to the
baseline, and a whole-crate run scales with the mutant count.
Long runs belong in the background with the output directory on disk; check free
space first, since a run writes one log and one diff per mutant.

## Reading the results

The output directory (default `mutants.out/`) holds `outcomes.json` plus
`caught.txt`, `missed.txt`, `unviable.txt`, `timeout.txt`, and per-mutant logs
and diffs under `log/` and `diff/`. The four outcomes mean different things:

- `caught` -- the suite failed on the mutant: that behavior is covered.
- `unviable` -- the mutant does not compile, so it carries no coverage signal. A
  high count is usually the warning-denied case above; confirm by looking for
  `warnings are denied` in the mutant's log.
- `missed` -- the suite passed on the mutant. This is a survivor, and each one
  needs a written reason: either the mutated behavior is reachable but
  unasserted (a test gap), or the mutation does not change behavior (an
  equivalent mutant). A survivor with neither explanation is an open finding.
- `timeout` -- the suite hung on the mutant.

## Worked example

The measured run for `crates/kallipai-adk/src/retry/decision.rs` reports:

```text
91 mutants tested in 8m: 74 caught, 17 unviable
```

All 17 unviable mutants are non-compiling by construction: fifteen substitute
`Default::default()` for a value of a type with no `Default` impl, and two
replace `&&` with `||` that does not type-check. The same file without
`CARGO_BUILD_WARNINGS=allow` reports `25 caught, 66 unviable`: 49 of those
unviable mutants are warning-denied builds that the environment variable turns
back into real tests, so the default environment reports 25 caught where the
variable reports 74.

Four survivors this file has produced, each a test gap rather than an equivalent
mutant, with what closed it:

- `shape_key`'s `TransportError::Deserialize` arm and the
  `serde_json::error::Category::Data` arm inside it — two mutants: deleting
  either one dropped the frame-shape key to `None` while the suite stayed green,
  so `shape_key_names_the_shape_class_leaf` asserts the frame site's key.
- `from_transport`'s `TransportError::Deserialize` arm, the SSE frame that
  failed to parse: deleting it downgraded that failure to `Unclassified`, so
  `typed_source_rows_of_the_table` carries the frame rows on both sides of
  `from_json_category`, including the `ProviderError::Transport` wrap the client
  produces.
- `from_status`'s `status.is_server_error()` guard, which decides the statuses
  no arm above it names: widening it to `true` reclassified a 405 as
  `Transient(TransientKind::Server)`, so `http_status_rows_of_the_table` pins an
  unenumerated 4xx beside a server error.

The pattern is the same each time: name the reachable behavior, assert it beside
the row that covers its neighbours, and rerun. The run quoted above reports no
survivors.

## Thresholds

There is no numeric threshold and no CI integration. A run is review input: each
survivor must be explained as a test gap or an equivalent mutant, and a gap
becomes either a test in the same change or a tracked item. The required bar for
a change stays the verification checklist in [Development](setup.md).
