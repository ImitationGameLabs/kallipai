---
name: Exploring
description: When you need to understand a codebase without necessarily changing it — the reader's survey from root and README through whatever docs and code the understanding goal requires
---

# Exploring — understand a codebase as a reader

Going from "I landed in an unfamiliar repo" to "I can explain what this
project is and how it works". This is the reader's survey: it reads
what serves *understanding* — README, explanatory docs, and the code
that illustrates how the project works — at whatever depth the goal
requires. It does not read contributor-only material (AGENTS.md,
CONTRIBUTING.md, and development guides — how-to-build and
how-to-contribute docs such as docs/development.md), because that
serves changing the code, not understanding it. If you are preparing to change code, load
`code/onboarding` instead.

## When to use

- You want to understand a project's purpose, design, and how it works
- You are evaluating a codebase (is it relevant? worth adopting?) and
  need a map before deciding

## When NOT to use

- You are about to write code in the codebase and need its conventions
  and constraints — that is `code/onboarding`.
- You already know a codebase and need a specific symbol — a targeted
  `grep`/`rg` beats a fresh survey.
- To investigate a specific bug or failure — use `code/debugging`
  for the systematic root-cause investigation workflow

## The sequence

**Survey the root.** Run `ls` on the repository root and read the
listing. The root names the stack (Cargo.toml, package.json, flake.nix,
go.mod), the key doc (README.md), and the top-level directories
(docs/, crates/, src/, packages/). This is the cheapest high-signal
step — it tells you what kind of project this is before you read
anything.
Done when:
- you can name the language/stack and the top-level directory layout

**Read the README.** The README states what the project is, what it
does, and how it is positioned — the one document written for exactly
the question you are asking.
Done when:
- you can state the project's purpose and its main capabilities

**Read what the understanding goal requires.** From here, read at the
depth your goal demands: docs that explain design and architecture
(usually under docs/), and code that illustrates how the parts work
together — entry points, a top-level module, a representative
subsystem. For code files, use `aifed outline` to see the symbol
structure first, then `aifed read <file> [start-end]` to read only the
relevant section — this is cheaper than reading the whole file and
respects the reader's goal of understanding, not exhaustively reading.
To follow a symbol you meet — what a call actually does, where a name
is defined or used — `aifed lsp def` / `aifed lsp refs` jump straight
there instead of re-grepping; the hashline you land on copies back
into `aifed read`.
Understanding a project often requires reading past the README; the rule
is to read what serves understanding, not what serves contribution.
Contributor-only documents (AGENTS.md, CONTRIBUTING.md, and development
guides such as docs/development.md) are for people changing the code and
add no value to a reader.
Done when:
- you can explain how the project works at the depth you set out to
  understand it

## If you are about to develop

This survey reads for understanding. If your goal is to change code in
the codebase, load `code/onboarding` — it builds on this base and adds
the contribution depth: AGENTS.md and CONTRIBUTING conventions,
development documentation, and task-area drill-in.

## Key behaviors to remember

- **Root-first, then README** — the root listing and the README are
  cheap and answer the reader's opening questions, because they are
  written for exactly this purpose.
- **Depth is set by the goal, not a fixed line** — a reader may need
  docs and code to genuinely understand a project, so read as deep as
  the understanding goal requires; the line that holds is *what* you
  read for — understanding, not changing.
- **Skip contributor-only material** — AGENTS.md, CONTRIBUTING.md, and
  development guides (how-to-build, how-to-contribute) encode
  constraints for people changing the code, which is noise for a
  reader; they are the one category that never serves understanding.
- **aifed is the reading tool, even read-only** — `outline` for
  structure, `read` for line-numbered ranges, `lsp` for symbol jumps;
  if the survey later turns into edits, the hashes are already in
  hand. `rg` stays the tool for searching where something lives.

## Anti-patterns

- **Reading contributor-only documents as a reader** — opening
  AGENTS.md or the development guides when you only want to understand
  the project, because their conventions are written for people
  changing the code; architecture and design docs — and the code
  itself — are fair game for a reader, these are not.
- **Deep-reading the whole codebase** — reading every file to
  understand a project, because context is bounded and most code is
  irrelevant even to a deep understanding; read what explains the
  parts you want to understand, not the whole tree.
