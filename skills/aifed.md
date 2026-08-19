---
name: AI-First Editor (aifed)
description: Prefer aifed first for any text or code work — exploring, reading, and editing files; run aifed --skill before your first edit
---

# aifed — Text & Code Work

Prefer aifed as the first tool you reach for when **working with files** — code, config, documentation, any text. It covers exploring structure (`outline`), reading content (`read`), and editing (`edit`). "Preferred first choice", not "one option among many".
For the survey workflow see `code/exploring` — aifed is the tool,
that skill is the process (including `aifed lsp` for symbol jumps,
which this file omits).

## Load the reference (required)

aifed ships its own complete, always up-to-date reference. **Before your first edit in a session**, load it, because the syntax (operators, locators, escaping rules, indent directives) is not guessable and operating without it leads to broken edits:

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
the result with `context_pin_last` (label
`skill:aifed-reference`).

When the editing-heavy work is done, `context_unpin skill:aifed-reference` to free context space.

This pattern keeps the detailed syntax (operators, locators, escaping rules, indent directives) available without re-running the command every turn, while avoiding context bloat when you're not editing.

## Key Behaviors to Remember

These are the things that commonly trip up agents — keep them in mind even without the full reference pinned:

- **Always read before edit** — hashlines (`LINE:HASH`) come from `aifed read`; you need current hashes to make any edit.
- **Prefer batch edits.** One heredoc with all operations avoids line-shift drift between sequential edits.
- **Never mix tools.** Don't alternate aifed with `cat`, `sed`, or other editors — it breaks hash verification on both sides.
- **Hash mismatch = retry.** If an edit fails because the hash doesn't match, re-read the file and retry with fresh hashes.
- **Outline first for large files.** `aifed outline <FILE>` gives you the structure before you dive into reading specific sections.

## When NOT to Use aifed

- A one-off glance at a value you are certain you will never edit (e.g. `cat`ing a single config value mid-command) — but if the reading might lead to edits, use `aifed read` anyway so the hashlines are already in hand.
- Binary files, images, non-text data.
- Creating a new file from scratch — `aifed edit` works on existing files; use shell redirect for initial creation.
