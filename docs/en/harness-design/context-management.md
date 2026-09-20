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
| `context_evict`  | Evict all working turns, replacing them with a summary that is pinned as `context_summary`. |
| `context_status` | Return a snapshot: pinned items with token counts, turn count, turn tokens.                 |

### `/compact` as a Special Case

The `/compact` command found in most coding agents maps directly to
`context_evict`: the agent writes a summary preserving key facts, and the tool
atomically pins the summary and evicts all turns. The agent decides what to
preserve: compaction is not a hidden heuristic but an explicit agent action.
