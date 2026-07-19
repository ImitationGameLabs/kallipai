---
name: Skill System
description: How to discover, navigate, create, and organize skills using the index-tree structure — the skill system's own organizational principles
---

# Skill Management

Skills are your accumulated experience, distilled into reusable guidance. This skill explains how to find the right skill fast, create effective ones, and keep the skill library organized.

## The Index Tree

Skills are organized as a navigable tree. Every category directory has an
`index.md` that acts as a local guide — what this category covers and how to
choose within it — and the root `skills/index.md` lists the top-level
categories. Read the root index for the live category layout; don't rely on a
memorized snapshot, because the tree grows as skills are added.

## How to Find a Skill

Navigate top-down, never blind-scan:

1. **Read root `index.md`** → identify which category matches your task
2. **Read `<category>/index.md`** → find the specific skill within that category
3. **Confirm with `kallip skill meta <category>/<name>`** → check description matches
4. **Load**: read the skill file, then in the next turn pin it with `context_pin_last` (kind `tool-result`, label: `skill:<name>`)

Each `index.md` answers one question: _"this directory covers what, and how do I pick?"_ Two reads max to locate any skill.

### Index files are transient — don't pin them

`index.md` files are navigation aids. Read them, use them to locate the skill you need, then let them go. They do not belong in pinned context. Only pin the actual skill you'll use across turns.

## Creating a Skill

### When to create

Create a skill when:

- You've repeated the same operation/pattern 2-3 times in a session
- You've gone through trial-and-error that future-you shouldn't repeat
- You've learned project-specific conventions worth preserving

Do **not** create a skill for:

- One-off tasks you'll never do again
- Things simple enough to not need guidance
- Reference content that a tool's own docs already provide (link to it instead)

### What makes a good skill

A skill should capture **judgment** — decisions, pitfalls, when-to-use guidance — not just syntax reference. The test: does this teach something that `--help` or official docs don't?

Principles:

- **Progressive disclosure** — point to authoritative references rather than duplicating them
- **Concise** — the body is pinned into context; every line costs tokens every turn
- **Action-oriented** — patterns, anti-patterns, decision rules
- **Reasoned, not imperative** — write the _reason_ and the _conditions_, not bare commands. "X works because Z; reconsider when Z doesn't hold" invites weighing; "always X / never Y" reads as doctrine and gets followed blindly. Imperative tone is the root cause of blind skill-adherence.
- **Integrate with agent capabilities** — explain how the skill connects to context management, pinning, other tools

> Note: the skills shipped with the repo predate this rule and are not yet retrofitted to it; that pass is tracked separately. Treat them as structural examples, not tone examples.

### Where to place it

Choose a category by asking: _what domain does this belong to?_

| Category | For                                                          |
| -------- | ------------------------------------------------------------ |
| `code/`  | Writing, editing, reviewing code; working with codebases     |
| `agent/` | Agent self-management (context, skills, subagents, workflow) |
| `ops/`   | Operations, tooling, infrastructure, external systems        |

Create a new subdirectory only when a category grows beyond ~6-8 skills. Depth limit: **two levels** (`category/skill.md`). Beyond that, navigation cost outweighs organization benefit.

### Naming

- File paths are kebab-case. The path relative to skills root (e.g. `agent/skill-management`) is the canonical identifier used for all lookups and routing.
- The `name` field in frontmatter is a display label — it can differ from the filename. The path is the identifier, not the name. If they were forced to match, `name` would be redundant.
- Verb or noun phrase describing the domain, not the tool (e.g. `testing`, not `cargo-test`)
- Nested paths use `/` separator: `code/testing`, `agent/skill-management`

## Skill Lifecycle

```
discover → read index → load & pin → use → unpin → (optionally) create → (optionally) promote
```

### Promote to shared

```bash
kallip skill promote submit <category>/<name>
```

Promote when the skill has proven value **beyond your own context** — when other agents working in the same project would benefit. The root agent reviews and approves.

## Updating Indexes

When you create or promote a skill:

1. Add an entry to the category's `index.md`
2. If the category is new, add it to the root `index.md`

An index that doesn't list its skills is worse than no index — it misleads.

## Skill Evolution

Skills are living documents. Review and restructure as they grow.

### Distillation signals

Create a skill when you notice:

- Repeated the same mistake or workaround 2+ times
- A costly detour you want to prevent next time
- Project conventions that can't be inferred from code alone

Don't create a skill for one-off tasks or things already well-documented elsewhere.

### When to split

- A skill covers multiple distinct scenarios AND exceeds ~80 lines
- Core principles can move to the meta skill (system prompt), detail stays in the file
- One section is always loaded but rarely needed (dead weight when pinned)

### When to merge

- Two skills are always loaded together
- Splitting created navigation overhead (agent must decide which to read)
- Content is tightly coupled and <30 lines each
