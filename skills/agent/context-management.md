---
name: Context Window Management
description: How to manage your own context window using built-in tools — pin, unpin, evict, and status — to stay effective across long sessions
---

# Context Management

Your context window is finite. Unlike traditional agents where compaction is a hidden heuristic, **you control your own attention** explicitly through tools. Use them actively — don't wait for automatic compaction to silently drop things you needed.

## The Tools

- `context_status` — check pinned items, turn count, remaining tokens
- `context_pin` / `context_unpin` — pin/unpin labeled content (survives eviction)
- `context_evict` — evict all turns, replacing them with a summary you write
- `context_pin_last` — pin a message already in your context by reference (a file you just read, a command output, your own last reply). Read the file first in a prior turn, then pin its tool result here (label: `skill:<name>`)

## Check Before You Act

Run `context_status` periodically — especially before large operations or when a session is getting long. Know your remaining budget so you can plan ahead rather than getting force-compacted at a bad moment.

## Pinning Strategy

Pin things you'll need **every turn** for the duration of a task:

- Skill references (e.g. `skill:aifed-reference` while doing heavy editing)
- Key decisions or constraints from the user
- Critical file state you're working with

**Unpin when the task shifts.** A pinned reference for a task you've finished is wasted context. Clean up after yourself.

Pinned content survives eviction — that's the point. If it's important enough to lose everything else, it belongs in a pin.

## Eviction: Evict on Your Terms

### When to evict

**The best time to evict is when switching topics or tasks** — this preserves
coherence within the current task while freeing attention for the next one.
Avoid evicting mid-task unless context is genuinely exhausted.

### Responding to threshold warnings

When you receive a system threshold warning (e.g. 50% context), **do not evict
reflexively**. The warning is advisory — it asks you to assess, not to act
blindly. Consider:

- Are you in the middle of a coherent task? If so, it's usually better to
  continue and evict at a natural boundary.
- Is there a topic shift coming soon? If so, wait for it.
- Is the context pressure real (approaching limits) or just at a checkpoint?
- Would unpinning stale items buy enough headroom without a full evict?

A threshold warning is a reminder to be mindful, not a trigger to panic.

Don't let automatic compaction decide what to keep. When you feel context getting heavy, **evict proactively** with a well-written summary:

The summary you provide to `context_evict` is pinned as `context_summary`. It becomes your working memory. Write it as if you'll have nothing else — because after eviction, you nearly don't.

A good eviction summary preserves:

- The current task and goal
- Key decisions made and their rationale
- Current state / progress
- File paths, IDs, and other locators you'll need
- Anything the user said that constrains future work

**Bad eviction** = losing the thread. **Good eviction** = a fresh context window with just enough to continue seamlessly.

## Anti-patterns

- **Never pin and forget.** Every pin costs tokens every turn. Audit periodically with `context_status`.
- **Don't wait for auto-compaction.** It uses a generic summarize strategy; your hand-written summary will always be better.
- **Don't pin transient data.** Tool outputs, intermediate results — these belong in working turns, not pins.
- **Don't evict reflexively at threshold warnings.** The 50% checkpoint is advisory — assess whether you're mid-task, near a natural boundary, or can reclaim space by unpinning instead.
