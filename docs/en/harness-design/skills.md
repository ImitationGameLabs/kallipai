---
title: Skill management
description: How distilled experience becomes context content.
order: 25
---

Skills are an application of [agentic context
management](context-management.md): experience distilled into markdown files,
read and pinned like any other context content.

## The Shared Skill Directory

A tagma has one shared skill directory. The root agent (the tagma's
single top-level agent, created at startup) is its sole author; any
other agent that wants a skill added or changed proposes it in
conversation to root.

## Skill File Format

A skill is a single markdown file with YAML frontmatter:

```markdown
---
name: my-skill
description: When and how to use this skill
---

Skill content here: tips, patterns, pitfalls.
```

The `name` and `description` fields in the frontmatter are metadata for the listing: loading strips the frontmatter, and only the body enters context.

The standard Agent Skill layout differs: the common practice renders one skill as a directory: `my-skill/` holds a required `SKILL.md` (likewise frontmatter metadata plus a body) alongside optional resources such as `scripts/`, `references/`, and `assets/`. This design keeps only the SKILL.md file; the per-skill directory collapses away. The reasoning is a simplification: most skills never need that structure, and imposing it on every skill would litter the tree with directories that hold a single SKILL.md, making skills harder to manage with the filesystem's natural tree. The compatibility path is clear: extending the index command to recognize the standard directory layout is the only future work; loading a skill is just reading a file, so a standard SKILL.md already works.

## Skill Discovery

`kallip skill index` scans a directory and turns every skill file's
frontmatter into a skill index, the entry point for discovering
skills. It is the key to progressive disclosure: the listing exposes
only each skill's name and one-line description, never the full file,
so the agent can judge which skills fit the situation before loading
anything. The index is generated fresh on every run, with no cache and no
index file, so the listing can never drift from what is on disk. It
renders two levels by default; `--depth 1` gives a flat one-level
view.

The command is documented in the [kallip CLI reference](../reference/kallip.md).
