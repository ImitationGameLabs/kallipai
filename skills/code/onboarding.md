---
name: Onboarding
description: When you are about to change code in a codebase and need its conventions and constraints — the development-depth exploration that builds on code/exploring with AGENTS.md, development docs, and task-area drill-in
---

# Onboarding — prepare to change code in an unfamiliar codebase

Going from "I understand what this project is" to "I know the
conventions that constrain my change and the exact code I will touch".
This is the contributor's survey: it builds on the reader's base
(`code/exploring`) and adds the development depth — agent/contributor
docs, development documentation, and targeted source reading.

## When to use

- You are about to write or modify code in a codebase and need its
  conventions and constraints first
- You need to find where a feature lives and how the area is meant to
  be changed

## When NOT to use

- You only want to understand a codebase without changing it — that is
  `code/exploring`; onboarding's depth is wasted context for a reader.
- You already know the codebase and its conventions — a targeted
  lookup beats re-onboarding.

## The sequence

**Run the base survey.** If you have not already, run the sequence in
`code/exploring` first: root listing, README, top-level map. This
skill adds depth on top of that base; skipping it means you miss the
project's purpose and stack while reading conventions that assume them.
Done when:

- the base survey from `code/exploring` is in context (or you run it
  now before continuing)

**Read the agent and contributor docs.** Read AGENTS.md if present —
it is written specifically for AI agents and states structure plus
decision rules, so it is usually the highest-value doc in the repo for
an agent about to write code. Read CONTRIBUTING.md if present for the
contribution conventions (branching, testing, review expectations).
These encode constraints the code alone does not show.
Done when:

- you know the conventions and decision rules that constrain your
  change

**Survey the development documentation.** Run `find . -name '*.md'`
(or list `docs/`) beyond the base survey. A `docs/` directory often
carries development, architecture, naming, and reference material —
look for the docs that govern your task, because they record decisions
(why the layout is what it is, what must not be changed casually) that
the code does not make obvious.
Done when:

- you know which development docs exist and which ones govern the task

**Refine the code map from AGENTS.md.** The base survey gave you the
top-level layout; AGENTS.md (when present) carries a detailed
directory tree naming each crate, module, or package and its role.
Read that tree and identify the entry points (src/main.rs,
src/lib.rs, packages/*/src/index.ts). Do not read source yet — the
goal is a map, not a deep read.
Done when:

- you can state the module / crate / package structure and where the
  entry points are, beyond what the base survey already showed

**Drill into the task area.** Read the code relevant to your task and
the conventions that govern it: the module layout, the types you will
touch, the error-handling and naming patterns in that area. This is
where the survey becomes targeted — deep-read the subtree you will
change, not the whole codebase. For code files, use `aifed outline` to
see the symbol structure first, then `aifed read <file> [start-end]` to
read only the relevant section rather than the whole file.
Done when:

- you have the specific files, types, or symbols your change needs
- you know the conventions (naming, error handling, module layout)
  that apply to the area you will change

## Key behaviors to remember

- **AGENTS.md is the agent's contract** — when present it is written
  for AI agents and states structure plus decision rules, so it is
  worth reading before any source; a repo can ship an AGENTS.md that
  the human-facing docs do not mention.
- **Docs before code for conventions** — development, architecture,
  and naming docs record why the code is the way it is, because code
  alone shows what is, not what must not be changed casually.
- **Broad then narrow** — enumerate and map first, then deep-read only
  the task-relevant subtree, because context is bounded and most of a
  codebase is irrelevant to any one change.

## Anti-patterns

- **Skipping the base survey** — jumping straight to AGENTS.md or the
  source without the root and README context, because you miss the
  project's purpose and stack that the conventions assume.
- **Reading the whole codebase** — trying to understand everything
  before writing anything, because context is bounded and most code is
  unrelated to your change; stop the broad survey once you can locate
  the task area.
- **Skipping AGENTS.md / CONTRIBUTING.md** — assuming conventions from
  the code alone, because these docs encode decisions (naming,
  structure, constraints) that the code does not make obvious.
- **Under-exploring the task area** — stopping at the map when you are
  about to change code, because the types and patterns that govern the
  change live in the source; drill into the subtree you will touch.
