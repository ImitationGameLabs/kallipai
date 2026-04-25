# Agentic Context Management

The most experimental design decision in just-agent: context management is not
a hidden heuristic — the agent controls its own attention explicitly through
tools.

## Design philosophy

Traditional agents manage context opaquely: when the context window fills, older
turns are silently summarized or dropped. The agent has no say in what stays or
goes.

just-agent takes a different approach: the agent gets **tools** to manage its
own context. It decides what to keep (`pin`), what to drop (`evict`), and what
to let go of (`unpin`). The traditional `/compact` operation becomes a special
case of this model, not the only option.

We openly acknowledge this approach is **unproven**. It may or may not
outperform traditional summarization-only strategies. But it enables patterns
that aren't possible when context management is opaque.

## Context layers

The `ContextStore` holds three layers, composed in priority order:

| Layer         | Content                | Behavior                                          |
| ------------- | ---------------------- | ------------------------------------------------- |
| Pinned layer  | Labeled items          | Always included. Survives eviction.               |
| Summary layer | Optional string        | Accumulated summary of compacted turns.           |
| Working turns | Chronological messages | Subject to eviction and compaction (newest last). |

Each turn is a `Vec<ChatMessage>` (assistant message + tool results) with a
pre-cached token estimate.

## The four context tools

| Tool             | What it does                                                                                |
| ---------------- | ------------------------------------------------------------------------------------------- |
| `context_pin`    | Add a labeled item to the pinned layer. Pinned items survive eviction and compaction.       |
| `context_unpin`  | Remove an item from the pinned layer. The content can then be evicted.                      |
| `context_evict`  | Discard the oldest N working turns. Hard delete — turns are not summarized.                 |
| `context_status` | Return a snapshot: pinned items with token counts, summary tokens, turn count, turn tokens. |

These tools go through the same policy system as shell tools. By default, they
are auto-allowed (no human approval needed).

## `/compact` as a special case

The `/compact` command found in most coding agents is just one possible use of
this model:

1. Produce a summary of the conversation so far.
2. Pin the summary.
3. Evict the original turns.

The pinned summary survives. But eviction is more general than compaction — the
agent might evict turns to switch focus to a different task, not just to shrink
context within the same task.

## Compaction strategies

When the token budget is exceeded (automatically checked each agent round),
compaction triggers. Three strategies are available:

### Evict

Drops all working turns outright. Preserves the existing summary. No LLM call
needed. The cheapest but most aggressive strategy.

### Summarize (default)

Makes an LLM call to summarize old turns. Builds input by incorporating the
existing summary first (for accumulation), then fills from oldest turns forward
until a token budget is exhausted. The new summary replaces the old one, and
all processed turns are dropped.

### Truncate

Iterates all turns, truncating individual messages that exceed a token limit
(default: 2000 tokens per message). Preserves turn structure. No LLM call
needed.

### Strategy selection

Configured via the `JUST_AGENT_COMPACT_STRATEGY` environment variable:
- `"evict"` → EvictStrategy
- `"truncate"` → TruncateStrategy
- anything else (default `"summarize"`) → SummarizeStrategy

## Automatic compaction in the agent loop

At the start of each agent round:

1. Compose context from all three layers.
2. Estimate prompt tokens.
3. If `prompt_tokens + output_reserve > context_window`, trigger compaction.
4. If compaction succeeds, re-compose and continue.
5. If compaction has nothing to compact, fall through (the round proceeds
   anyway).

If compaction fails, drained turns are **restored** — no data loss on failure.

## Emergent skills

Skills are a natural consequence of agentic context management:

1. The agent accumulates experience — effective patterns for a CLI tool, debugging
   strategies, project-specific conventions.
2. It distills that experience into a markdown file:
   `.just-agent/skills/<name>/SKILL.md` (with optional YAML frontmatter).
3. When it encounters a matching situation later, it reads the file and pins
   the content into context.
4. When the skill is no longer needed, `context_unpin` removes it.

No dedicated skill system is needed. File read + pin naturally forms skill
management — the same primitives the agent already uses for any other context
content.

### Skill file format

```
.just-agent/skills/
└── my-skill/
    └── SKILL.md
```

```markdown
---
name: my-skill
description: When and how to use this skill
---

Skill content here — tips, patterns, pitfalls.
```

The YAML frontmatter is stripped on load; only the body is pinned into context.

### Meta-skill

A built-in meta-skill called `novel-task` is auto-created on first run. It
provides guidance for approaching unfamiliar situations: gather information
broadly, verify assumptions incrementally, ask for help when uncertain, and
distill experience into new skills after the fact.
