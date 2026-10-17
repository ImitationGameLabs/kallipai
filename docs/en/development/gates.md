---
title: Gate runs and gate archives
description: The canonical recipe for running a verification gate and recording it as evidence.
order: 35
internal: true
---

A gate is one command whose exit status is treated as evidence. The
command alone is not enough to reproduce a verdict: the working
directory and the build directory both change the outcome. This page
fixes those two inputs, and the shape of the archive that records them.
To check a change rather than record it, run the matching command from
[the table below](#the-gates); run it from the repository root. The archive
protocol later on this page is for runs whose result must stand as evidence.

The collector, `gate.sh`, lives in the team workspace that contains this
checkout, not in this repository; every `gate.sh` on this page means that
script. The archives it writes are described in
[Recording a gate archive](#recording-a-gate-archive).

## Working directory

Run gates from the repository root. A run from the wrong directory looks like
a content failure: `deno task` cannot find `deno.json` or `package.json` and
exits 1. `gate.sh` performs the `cd` itself and records it as `cwd=`.

## Build directory: never share one

Cargo unit hashes follow the package name and version, not the commit, so two
checkouts sharing one `CARGO_TARGET_DIR` write the same unit names. A run in
one tree can then reuse what the other built — a false red when the review
copy is older, and, worse, a false green: a tree whose tests should fail
reports a pass because the binary came from elsewhere. The trigger is the
source mtime being older than the other tree's build.

- Give each tree its own target directory. `gate.sh` defaults to
  `/tmp/gate-target/<repo name>-<sha1 of the repo's realpath, first 12>`, so
  two same-named checkouts stay apart.
- If `CARGO_TARGET_DIR` is already set in the environment, `gate.sh` does not
  override it: it records `target=` and a `target-note=WARNING` line. The
  artifacts are then not private to this tree, so a run without
  `--require-new-build` can pass on what another tree built; a run with the
  flag still forces a rebuild, so its freshness verdict holds.

## The gates

| Surface | Command |
| --- | --- |
| Rust format | `cargo fmt --all --check` |
| Rust lint | `cargo clippy --workspace --all-targets --all-features` |
| Rust test | `cargo test --workspace --all-targets --all-features --no-fail-fast` |
| Rust docs | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps` |
| Markdown | `deno task fmt:md:check` (read-only; `deno task fmt:md` rewrites) |
| Frontend format | `deno task fmt:check` (prettier `--check` plus `rumdl fmt --check`) |
| Frontend lint | `deno task lint` |
| Frontend types | `deno task check` |
| Frontend tests | `deno task test` |
| Nix format | `nixfmt --check <files>` (`nixfmt <files>` writes) |
| Nix lint | `statix check flake.nix && statix check nix/` |
| TOML | `taplo format --check <file>` (`taplo fmt <file>` writes; never bare) |

Notes:

- `--no-fail-fast` keeps the first failing suite from hiding the others. It is
  part of the recorded command line, not something the collector adds.
- Clippy, test, and doc runs deny warnings through `.cargo/config.toml`
  (`build.warnings`); the doc command spells the flag out on its command
  line as well. Never relax that (`CARGO_BUILD_WARNINGS=allow`) to obtain a
  green archive.
- Markdown belongs to rumdl, not prettier (`.prettierignore` excludes
  `**/*.md`); rumdl does not align table columns, so a one-row edit never
  reflows the rest of a table.
- A page keeps its title in the frontmatter `title` and opens the body with
  prose. rumdl's MD025 counts a level-one heading in the body as a second
  top-level heading and fails the gate; the other pages under `docs/en/` are
  laid out the same way.

## Freshness: what counts as a new build

A cached `cargo clippy` can exit 0 without building anything, so `rc=0` alone
proves neither that this run compiled the audited sources nor that the
artifacts came from this tree. A cached run may even replay earlier warnings
and, with `build.warnings` set to deny, fail on its own, but a pass still says
nothing about a build. A build counts as fresh when the archive shows a build
event for every workspace member:

- The events are `Compiling`, `Checking`, and `Documenting`. `cargo test`
  names the packages it compiles, `cargo clippy` names them as `Checking`,
  and `cargo doc` emits `Documenting` for library and binary targets alike —
  a binary-only member still appears there.
- Take the package names from those lines and intersect them with the
  workspace member package names, read from the root `Cargo.toml` members list
  and each member's `[package] name` — not from directory names, which differ
  for some members. The intersection must cover every member (33 members at
  the time of writing; the gate reads the number and this page only records
  the reference value).
- Dependencies emit the same events; the intersection drops them, so counting
  raw event lines is not a substitute.
- A warm private target directory makes cargo skip the build, and then no
  event is emitted at all: the run cannot prove it built anything. That is
  why the collector touches the mtime of every `.rs` file under the member
  directories before a `--require-new-build` run. The touch is a requirement
  of the check, not a convenience: it changes no content (tree and index stay
  clean), skips dependencies, and leaves a line beginning `touch=` naming
  how many member `.rs` files it touched (`touch=<n> member .rs files …`).

`gate.sh --require-new-build` performs the check and writes
`freshness=fresh(<hits>/<members>)` or `stale(...)`, plus
`freshness-seen=<package,...>` naming the members that were seen building —
the input of the verdict, so it can be checked without the full log. The
denominator is the number of hits the run expects: by default the workspace
member count, `N` when it is given explicitly, and the distinct
non-blank entry count of the list passed to `--members-from` (an
explicit `N` wins over the list).
A recorded run leaves `N` off; a smaller N or a short member list narrows the
check while the archive looks just as green. A stale archive exits 103, so it
cannot pass as green. `--allow-stale` keeps the run green but leaves
`freshness-exempt=1`: use it for a limitation the run itself cannot remove,
never to hide a cached verdict.

## Recording a gate archive

```sh
/abs/path/tools/gate.sh -r <repo> -o <outdir> -R <round> [-t <secs>] [--require-new-build [N]] <gate-name> -- <cmd...>
/abs/path/tools/gate.sh --list -o <outdir> <name-prefix>
# absolute path on purpose: gate.sh lives outside this repository and is not on PATH
```

- Stage before running. `stamp=` is `git write-tree`, so it always describes
  the index and never the working tree. `dirty=` counts `git status --porcelain`
  entries, untracked and unstaged alike, so a change whose edits are all
  staged still reports `dirty>0`.
  An entry is inside `stamp` only when its index column (the first) holds
  `A`, `M`, `D`, `R`, `C`, or `T`; a space or `?` there means it is outside.
- Short options (`-r`, `-o`, `-R`, `-t`) must precede the gate name: the
  parser stops at the first positional argument, so `-t 3600` written after the
  gate name is taken as the command, and the run dies with `expected -- before
  command`. Long options (`--require-new-build`, `--allow-stale`, `--no-touch`,
  `--target-dir`, `--members-from`) are accepted anywhere before `--`.
- The header carries `gate`, `round`, `head`, `stamp`, `dirty`, `legend`,
  `cmd`, `cwd`, `env`, `tool`, `target`, and `start` (`env` and `tool` are
  informational: nothing on this page depends on them); the footer carries
  `rc`/`end`, `totals:` for test runs, and `freshness:`/`freshness-seen:`
  when requested. A `--require-new-build` run also records the `touch=` line.
- `--no-touch` skips the mtime touch. It exists for self-tests that only
  synthesise build output, and is never right for a recorded gate.
- A failing run keeps the full output in `<archive>.full` and puts excerpts in
  the archive.
- One round marker per round. Re-running a round overwrites that round's
  archive, which is the point; a fix round must use a new marker. Enumerate a
  round family with `gate.sh --list` — it matches a literal prefix, so
  it also finds `site-s4r4-…` where a `site-s4-*` glob would not.
- Exit codes: the command's own status; 124 when `-t` fires; 103 for a
  failed freshness check; 2 for a bad argument or a precondition it cannot
  meet (`-r` not a git checkout, an uncreatable target directory, an
  unparsable members list). A failed freshness check wins over the command's
  own status: the process exits 103 while the archive keeps the command's rc
  (124 when `-t` fired).

## After an i18n change

`deno task i18n` compiles the message catalogs under
`packages/kallipai-ui/i18n/project.inlang/` into
`packages/kallipai-ui/src/paraglide/`. Of the sources there,
`settings.json` is the project configuration — base locale, locales,
module list, message path pattern — and is hand edited: a compile
leaves it byte for byte alone.

`.meta.json`, `README.md` and the folder's `.gitignore` are tool
products, and all three are tracked. `.meta.json` and `README.md` are
`paraglide-js init` boilerplate; the `.gitignore` keeps `cache/` out of
this repository. A compile leaves an existing copy alone; what it
rewrites follows the trigger: an installed SDK newer than the version
recorded in `.meta.json` rewrites all three; otherwise a missing
`.gitignore` or `README.md` comes back by itself, and a missing
`.meta.json` rewrites all three. `cache/` plays no part.

The `.gitignore` the tool writes is its own template, which ignores
everything but `settings.json`: new files under `messages/` become
invisible to git, and no gate looks at that file, so a green run says
nothing about it. So when a newer SDK rewrites the three, stage only the
refreshed `.meta.json` — it records the migration — and restore
`README.md` and `.gitignore` from `HEAD`: committing the template would
hide new files under `messages/`. A rewrite that fills in a missing
`README.md` can differ from the committed copy, because the two came
from different tool versions.

After a compile that filled in a missing file, check the folder with
`git status`. Take nothing from the index, which may already hold the
rewrite. Restoring from `HEAD` also brings the recorded version
back: a rewritten `.meta.json` records the installed one. That holds
while the rewrite is uncommitted. If it did go in, take `README.md` and
`.gitignore` from the commit before it (`git checkout <rev> -- <paths>`).
Otherwise, if the installed SDK is not newer than the recorded
version, restore the three files from `HEAD` and rerun the gate:

```sh
git checkout HEAD -- packages/kallipai-ui/i18n/project.inlang/.gitignore
git checkout HEAD -- packages/kallipai-ui/i18n/project.inlang/.meta.json
git checkout HEAD -- packages/kallipai-ui/i18n/project.inlang/README.md
```
