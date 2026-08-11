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

Examples of what to pin for the duration of a task (per the first rule):

- Skill references (e.g. `skill:aifed-reference` while doing heavy editing)
- Key decisions or constraints from the user
- Critical file state you're working with

A pinned reference for a task you've finished is wasted context — unpin it (per the second rule) and clean up after yourself.

Pinned content survives eviction — that's the point. If it's important enough to lose everything else, it belongs in a pin.

## Decision rules

- If you'll need it every turn for the duration of a task, then pin it, because pinned content survives eviction.
- If the task has shifted and the pin is stale, then unpin, because every pin costs tokens every turn.
- If you're switching topics or tasks, then evict with a hand-written summary, because auto-compaction uses a generic strategy and your summary preserves the thread.
- If a threshold warning fires mid-coherent-task, then continue to the next natural boundary rather than evicting reflexively, because evicting mid-task loses the thread.

These rules decide; the sections below elaborate them with examples.

## Eviction: Evict on Your Terms

### When to evict

Evict at topic or task switches (per the third rule) — it preserves coherence
within the current task while freeing attention for the next one. Avoid evicting
mid-task unless context is genuinely exhausted.

### Responding to threshold warnings

When a threshold warning fires, assess before acting (see the rule above). Consider:

- Are you in the middle of a coherent task? If so, it's usually better to
  continue and evict at a natural boundary.
- Is there a topic shift coming soon? If so, wait for it.
- Is the context pressure real (approaching limits) or just at a checkpoint?
- Would unpinning stale items buy enough headroom without a full evict?

A threshold warning is a reminder to be mindful, not a trigger to panic.

Don't let automatic compaction decide what to keep. When you feel context getting heavy, **evict proactively** with a well-written summary:

The summary you provide to `context_evict` is pinned as `context_summary`. It becomes your working memory. Write it as if you'll have nothing else — because after eviction, you nearly don't.

For the dimensions and quality criteria that make an eviction summary reliable,
see `what-to-preserve` — it defines what must survive (task, decisions and
rationale, state, locators, user constraints, rejected approaches, blockers)
and how to write each entry so the summary is usable after eviction.
For sessions with critical decisions or locators at stake, see `preserving-context`
for a multi-round summarization process that pins each round before
evicting, so nothing is lost to a single pressured summary pass.

**Bad eviction** = losing the thread. **Good eviction** = a fresh context window with just enough to continue seamlessly.

## Anti-patterns

- **Never pin and forget.** Every pin costs tokens every turn. Audit periodically with `context_status`.
- **Don't wait for auto-compaction.** It uses a generic summarize strategy; your hand-written summary will always be better.
- **Don't pin transient data.** Tool outputs, intermediate results — these belong in working turns, not pins. (The pinned root skill index — the output of `kallip skill index`, label `skill:index` — is the exception: it is a reference you reuse every turn, not transient working data.)
- **Don't evict reflexively at threshold warnings.** The 50% checkpoint is advisory — assess whether you're mid-task, near a natural boundary, or can reclaim space by unpinning instead.
