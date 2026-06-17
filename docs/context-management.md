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

The `ContextStore` holds two layers, composed in priority order:

| Layer         | Content                | Behavior                                          |
| ------------- | ---------------------- | ------------------------------------------------- |
| Pinned layer  | Labeled items          | Always included. Survives eviction.               |
| Working turns | Chronological messages | Subject to eviction and compaction (newest last). |

Each turn is a `Vec<ChatMessage>` (assistant message + tool results) with a
pre-cached token estimate.

## The four context tools

| Tool             | What it does                                                                                |
| ---------------- | ------------------------------------------------------------------------------------------- |
| `context_pin`    | Add a labeled item to the pinned layer. Pinned items survive eviction and compaction.       |
| `context_unpin`  | Remove an item from the pinned layer. The content can then be evicted.                      |
| `context_evict`  | Evict all working turns, replacing them with a summary that is pinned as `context_summary`. |
| `context_status` | Return a snapshot: pinned items with token counts, turn count, turn tokens.                 |

These tools go through the same policy system as shell tools. By default, they
are auto-allowed (no human approval needed).

## `/compact` as a special case

The `/compact` command found in most coding agents maps directly to
`context_evict`: the agent writes a summary preserving key facts, and the tool
atomically pins the summary and evicts all turns. The agent decides what to
preserve — compaction is not a hidden heuristic but an explicit agent action.

## Compaction

When the token budget is exceeded (automatically checked each agent round),
compaction triggers using a summarize strategy:

Makes an LLM call to summarize old turns. Builds input by incorporating the
existing summary first (for accumulation), then fills from oldest turns forward
until a token budget is exhausted. The new summary replaces the old one, and
all processed turns are dropped.

The maximum summary token count is configured via the
`JUST_AGENT_SUMMARY_MAX_TOKENS` environment variable (default: 1200).

## Automatic compaction in the agent loop

At the start of each agent round:

1. Compose context from both layers (pinned, turns).
2. Estimate prompt tokens.
3. If `prompt_tokens + output_reserve > context_window`, trigger compaction.
4. If compaction succeeds, re-compose and continue.
5. If compaction has nothing to compact, fall through (the round proceeds
   anyway).

Here `context_window` is the active profile's declared `max_context_window` (per-profile; see
[Model Profiles](./reference/env.md#model-profiles)), while `output_reserve` and the rest of the
budget shape are global env policy.

If summarize_and_evict fails, the store is unchanged — no data loss on failure.

## Emergent skills

Skills are a natural consequence of agentic context management:

1. The agent accumulates experience — effective patterns for a CLI tool, debugging
   strategies, project-specific conventions.
2. It distills that experience into a markdown file:
   `<data-dir>/just-agent/skills/<name>.md` (with optional YAML frontmatter).

   The data directory is determined by `JUST_AGENT_DATA_DIR` env var, or the
   platform default if unset:

   | Platform | Default path                               |
   | -------- | ------------------------------------------ |
   | Linux    | `~/.local/share/just-agent`                |
   | macOS    | `~/Library/Application Support/just-agent` |
   | Windows  | `%APPDATA%\just-agent`                     |

3. When it encounters a matching situation later, it reads the file and pins
   the content into context.
4. When the skill is no longer needed, `context_unpin` removes it.

No dedicated skill system is needed. File read + pin naturally forms skill
management — the same primitives the agent already uses for any other context
content.

### Skill file format

```
<data-dir>/just-agent/skills/
└── my-skill.md
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

A built-in meta-skill called `bootstrap` is compiled into the binary and
appended to the system prompt at agent spawn time. It teaches the agent how
to discover, load, and create skills, along with behavioral guidelines for
approaching unfamiliar situations.
