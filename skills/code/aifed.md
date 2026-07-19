---
name: AI-First Editor (aifed)
description: When to use aifed and how to integrate it with agent context management for text editing and coding tasks
---

# aifed — Text Editing & Coding

Use this skill whenever your task involves **reading, writing, or modifying files** — code, config, documentation, any text.

## Getting Started

aifed ships its own complete, always up-to-date reference. Before your first edit in a session, load it:

```bash
aifed --skill
```

This is **progressive disclosure**: this skill tells you _when_ and _how_ to use aifed; `aifed --skill` gives you the _full syntax reference_. You don't need to memorize everything — just know to run it.

## Pinning for Focused Work

If your current task is primarily text editing or coding — not just a one-off file tweak — **pin the `aifed --skill` output into context** so it stays available across turns:

```bash
# Load the full reference into context as a pinned skill
aifed --skill > /tmp/aifed-skill.md
```

Then read the file (e.g. `cat /tmp/aifed-skill.md`), and in the next turn pin
the result with `context_pin_last` (kind `tool-result`, label
`skill:aifed-reference`).

When the editing-heavy work is done, `context_unpin skill:aifed-reference` to free context space.

This pattern keeps the detailed syntax (operators, locators, escaping rules, indent directives) available without re-running the command every turn, while avoiding context bloat when you're not editing.

## Key Behaviors to Remember

These are the things that commonly trip up agents — keep them in mind even without the full reference pinned:

1. **Always read before edit.** Hashlines (`LINE:HASH`) come from `aifed read`. You need current hashes to make any edit.
2. **Prefer batch edits.** One heredoc with all operations avoids line-shift drift between sequential edits.
3. **Never mix tools.** Don't alternate aifed with `cat`, `sed`, or other editors — it breaks hash verification on both sides.
4. **Hash mismatch = retry.** If an edit fails because the hash doesn't match, re-read the file and retry with fresh hashes.
5. **Outline first for large files.** `aifed outline <FILE>` gives you the structure before you dive into reading specific sections.

## When NOT to Use aifed

- Quick file inspection where you won't edit (e.g., checking a config value) — `cat` is fine for read-only.
- Binary files, images, non-text data.
- Creating a new file from scratch — `aifed edit` works on existing files; use shell redirect for initial creation.
