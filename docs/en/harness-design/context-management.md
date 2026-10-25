---
title: Agentic Context Management
description: How agent context is assembled, compressed, and persisted.
order: 20
---

Context management in KallipAI is explicit: the agent controls its own
attention through tools.

## Design Philosophy

Context compaction usually works one of two ways: a manual /compact
command, or auto-compaction: a simple threshold mechanism where the agent
harness triggers a compaction once the context reaches the limit.

KallipAI implements agentic context management: with the `pin`, `unpin`, and
evict context primitives, the agent manages its own attention, deciding
what should stay in context. This is in some ways closer to how a person
manages attention: while working on something, you know which things to
keep attending to until the task is done.

A /compact-style manual compaction is still available: tell the agent "you
need to compact your context now"; the agent will almost never refuse the
reminder and then triggers the compaction itself.

One benefit of this mechanism is timing: the agent knows when compaction
fits, such as the gap between one task ending and the next beginning,
while threshold-triggered auto-compaction often lands in the middle of a
running task. Threshold-triggered compaction should be a safety net, not
the regular mechanism.

### Context Layers

The context store holds two layers, composed in priority order:

| Layer         | Content                | Behavior                                          |
| ------------- | ---------------------- | ------------------------------------------------- |
| Pinned layer  | Labeled items          | Always included. Survives eviction.               |
| Working turns | Chronological messages | Subject to eviction and compaction (newest last). |

Each turn is a `Vec<Message>` (assistant message + tool results) with a
pre-cached token estimate.

Oversized external messages are size-guarded where they enter the working
turns: beyond the entry cap they are cut to a head+tail slice with a banner
pointing at the spilled original, while the request-time compaction
(summarizing older turns when the budget overflows) stays the
budget-management layer. The two guards compose; neither replaces the
other.

### The Four Context Tools

| Tool             | What it does                                                                                |
| ---------------- | ------------------------------------------------------------------------------------------- |
| `context_pin`    | Add a labeled item to the pinned layer. Pinned items survive eviction and compaction.       |
| `context_unpin`  | Remove an item from the pinned layer. The content can then be evicted.                      |
| `context_evict`  | Keep the newest turns, condense the older ones into a pinned `context_summary`.            |
| `context_status` | Return a snapshot: pinned items with token counts, turn count, turn tokens.                 |

### `/compact` as a Special Case

The `/compact` command found in most coding agents maps directly to
`context_evict`: the agent writes a summary preserving key facts, and the tool
atomically pins the summary and evicts the turns older than the keep window.
The window itself is configuration, not a tool argument: a turn count plus a
share of the effective budget, so the agent cannot widen it from a call. The
token cap derives from the active window like the pinned budget does, so a
profile that changes the window moves it too; the turn count is a configured
number. Configuration is the tagma-wide default plus, per agent, the two fields
a spawn request may carry (`context_keep_turns`, `context_keep_tokens_ratio`),
so a supervisor can hand one subagent a tighter window without moving the
default for the rest. The agent decides what to preserve: compaction is not a
hidden heuristic but an explicit agent action.

Every eviction replaces the previous `context_summary`: there is never a second
summary, so the new text has to carry the facts the old one held. Eviction is a
retention rule rather than a budget guarantee — the newest window is kept for
being newest, not for fitting under a warning threshold, so a context can sit
above a threshold right after an eviction.

The summary states the range it covers, and only a manual eviction writes that
line: the tool computes it from the store, marks it as its own, and automatic
compaction strips it before carrying the text on, because the range described
the store at one moment rather than a lasting fact. The line names the boundary
turn and counts the survivors instead of writing a span — pinned turns draw ids
from the same counter as conversation turns, so a span would claim turns that
never existed. The reply reports the window it applied: a configured turn cap of
0 condenses everything and says so in a note, while a newest turn that alone
exceeds the token slice is reported as degraded.
