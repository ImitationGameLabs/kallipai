---
name: Skill Index
description: Root navigation index for all skills — pick a category, then drill down
---

# Skill Index

Skills are organized by domain. Read this to pick a category, then read that category's `index.md` to find a specific skill.

## Categories

| Directory | Covers                                                               | When to look here                                                                               |
| --------- | -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `agent/`  | Agent self-management: CLI, context, skills, subagents, coordination | Anything about managing your own runtime, context window, subagents, or the skill system itself |
| `code/`   | Working with code and text: editing, testing, debugging              | Writing, reading, or modifying files; codebase exploration                                      |
| `ops/`    | _(reserved)_ Operations, tooling, infrastructure                     | External systems, CI/CD, deployment _(no skills yet)_                                           |

## How to navigate

1. Read this index → pick a category
2. Read `<category>/index.md` → pick a skill
3. `kallip skill meta <category>/<name>` → confirm
4. read the skill file, then `context_pin_last` (kind `tool-result`) → load and start working

This index is a navigation aid — read it, don't pin it.
