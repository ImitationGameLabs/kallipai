---
name: Skill System
description: How to discover, navigate, and organize skills using the index-tree structure — the skill system's own organizational principles
---

# Skill Management

Skills are your accumulated experience, distilled into reusable guidance. This skill explains how to find the right skill fast, create effective ones, and keep the skill library organized.

## The Index Tree

Skills are organized as a navigable tree of directories. There are no
hand-maintained index files — the index is **generated on demand** by
`kallip skill index <dir>` from each skill's frontmatter, so the listing is
always consistent with the files. A category directory carries a `README.md`
whose frontmatter `description` is what the generated index shows for that
category.

## How to Find a Skill

Navigate top-down, never blind-scan:

- **Index the root.** `kallip skill index <skills-path>` → see the root-level skills and the top-level categories.
- **Index the category.** `kallip skill index <skills-path>/<category>` → see the skills in that category (skip this for a root-level skill).
- **Confirm.** `kallip skill meta <skills-path>/<id>` → check the description matches (`<id>` is the path relative to skills root: `aifed` or `agent/kallip`).
- **Load.** Read the skill file, then in the next turn pin it with `context_pin_last` (kind `tool-result`, label: `skill:<name>`).

Each index answers one question: _"this directory covers what, and how do I pick?"_ Two index runs max to locate any skill.

### The root index is pinned; category indexes are transient

The root index (`kallip skill index <skills-path>`) is your always-on map of the library — the bootstrap floor tells you to run it once and pin the output (label `skill:index`) so it persists across turns and you never start a task blind to what skills exist. A category index is different: run it transiently to locate a skill, then let the result go — it does not belong in pinned context. Beyond the root index, only pin the actual skill you'll use across turns.

## Creating a Skill

For the step-by-step workflow to create one from scratch, see `skill/creating-a-skill`. This section covers the _whether_ and _where_.

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

For kallip itself, command syntax lives in `kallip --reference` (auto-generated
from the binary, so it never drifts) — a kallip skill teaches _when_ and _why_,
never the flag list. Pin the reference output (label `kallip:reference`) when
the work spans many commands.

For how to write the content — the description that gets a skill loaded, the
Process/Reference body structures, and the reasoned-not-imperative voice
(imperative tone is the root cause of blind skill-adherence) — see `skill/what-makes-a-good-skill`.

### Where to place it

First decide: **root or a category?**

- **Root** for operational primitives you reach for across most work sessions — skills so fundamental that burying them in a category costs a drill-in every time (e.g. `aifed` for editing, `context-management` for context hygiene). Keep the root small: a handful of cross-cutting skills, not a dumping ground.
- **A category** for skills that belong to a recognizable domain. Pick it by asking: _what domain does this belong to?_

| Category | For                                                          |
| -------- | ------------------------------------------------------------ |
| `code/`  | Writing, editing, reviewing code; working with codebases     |
| `agent/` | Agent self-management (the kallip CLI, subagents, tagma coordination) |
| `skill/` | The skill system — authoring, creating, reviewing, organizing skills |

Create a new category only when a domain has ~6-8 skills (the `skill/` category is the self-referential exception — the skill system is a coherent domain that earns its own category regardless of count). Depth limit: **two levels** (`category/skill.md`). Beyond that, navigation cost outweighs organization benefit.

### Naming

- File paths are kebab-case. The path relative to skills root is the canonical identifier used for all lookups and routing — `aifed` for a root skill, `skill/skill-management` for a categorized one.
- The `name` field in frontmatter is a display label — it can differ from the filename. The path is the identifier, not the name. If they were forced to match, `name` would be redundant.
- Categorized paths use `/` separator: `code/testing`, `skill/skill-management`
- For the wording craft — name the domain, not the tool (`testing`, not `cargo-test`); write `name` + `description` as one readable index entry — see `skill/what-makes-a-good-skill`.

## Skill Lifecycle

```text
discover → kallip skill index → load & pin → use → unpin → (optionally) author, review, improve, or propose
```

### Authoring and sharing skills

There is one tagma-wide skill collection, in the shared skill directory, and
only the **root agent** can write it. If you are not root, you cannot write
skill files yourself: a skill you want added to the shared collection must be
**proposed in conversation to the root agent** — paste the new or changed
content and explain why it is worth sharing. The root agent reviews and authors
it.

If you are the root agent, you author shared skills directly. The shared
directory is the `skills path` in your identity facts; write the file there
with `bash_exec`. To stay crash-safe, write to a temp file in that directory
and `mv` it into place rather than redirecting `>` directly — a half-written
`.md` would otherwise be left on a crash:

```bash
# SHARED = the `skills path` from your identity facts.
# Place a primitive at root (`$SHARED/my-skill.md`), or a domain skill in a
# category (`$SHARED/<category>/my-skill.md`; mkdir -p the category first).
mkdir -p "$SHARED/<category>"   # omit for a root-level skill
cat > "$SHARED/<category>/my-skill.md.tmp" <<'EOF'
---
name: My Skill
description: ...
---
...body...
EOF
mv "$SHARED/<category>/my-skill.md.tmp" "$SHARED/<category>/my-skill.md"
```

(The `bootstrap` name is reserved for the compiled-in meta-skill and cannot be
used for a shared file.)

No index update is needed when you add a skill — `kallip skill index` generates
the listing from each file's frontmatter, so a new skill file (or a new
category directory with a `README.md`) appears automatically.

## Skill Evolution

Skills are living documents. Review and restructure as they grow — the review workflow lives in `skill/reviewing-a-skill`, and changing an existing skill in `skill/improving-a-skill`.

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
