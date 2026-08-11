---
name: Preserving Context
description: When your context is approaching limits and you need to evict turns — the multi-round summarization process that ensures critical information survives eviction
---

# Preserving Context — multi-round summarization before eviction

A single-pass summary written under pressure — when context is nearly full
— is the summary most likely to lose something critical: a decision's
rationale, a file locator, a constraint the user stated once. This skill
spreads the summarization across multiple rounds, pinning each round's
output so it survives the eviction that follows. The pinned summaries
become the new working memory; even if one round missed something, the
others may have caught it.

## When to use

- When you are about to `context_evict` and the session contains decisions,
  state, or locators you cannot afford to lose
- When a threshold warning fires and you have reached a natural task
  boundary where eviction is appropriate

## When NOT to use

- For a short session with no significant decisions — a single summary in
  `context_evict` suffices, because there is little to lose
- Mid-task when you can reclaim space by unpinning stale items instead —
  see `context-management` for the eviction-decision rules

## The sequence

**Scan and extract.** Read through your context and extract every
dimension from `what-to-preserve` (task and goal, decisions and
rationale, current state, locators, user constraints, rejected
approaches, open blockers). Follow the quality criteria there —
structured sections, rationale over conclusion, concrete references,
explicit emptiness. Pin the result (label: `evict:round-1`).
Done when:

- every checklist category has been scanned and either filled or marked empty
- the summary is pinned as `evict:round-1`

**Review and supplement.** Re-read your context with the pinned round-1
summary beside it. Look for what round 1 missed: an aside the user dropped,
a tool output with a locator, a rejected approach that matters because
someone might suggest it again. Add supplements as a second pin (label:
`evict:round-2`). This round is not a re-summary — it is a gap check,
because the first pass under pressure always misses something.
Done when:

- you have re-scanned context against the round-1 pin
- supplements are pinned as `evict:round-2` (or you confirmed round 1 was complete)

**Evict with confidence.** Call `context_evict` with a summary built from
the pinned rounds — synthesized from pinned material, not re-derived
from raw turns. After eviction, the pinned rounds remain
as a safety net alongside the `context_summary` pin.
Done when:

- `context_evict` has run with a summary synthesized from the pinned rounds
- the pinned rounds survive the eviction (they are pins, not turns)

## Key behaviors to remember

- **Pin before evict, not after.** Pins survive eviction; turns do not —
  writing the summary into a turn and then evicting loses it, because the
  turn is exactly what gets evicted.
- **The checklist is the safety net.** Structured categories (the
  dimensions from `what-to-preserve`) catch more than freeform
  summarization, because a category marked empty is a conscious
  decision, not an oversight.
- **Two rounds, not five.** The second round is a gap check; further rounds
  hit diminishing returns and burn tokens you may need for the task itself,
  because the value of a third pass rarely exceeds the context it costs.
- **Clean up the pins after the task stabilizes.** Once the new context is
  established and the post-eviction task is underway, unpin the `evict:*`
  pins, because they were scaffolding for the transition, not permanent
  reference material.

## Anti-patterns

- **Single-pass evict under pressure** — writing one summary in the
  `context_evict` call when context is nearly full, because that is the
  moment most likely to drop a critical detail; the multi-round process
  exists to move the summarization out of that pressure.
- **Prose summary without structure** — a freeform paragraph, because
  unstructured prose lets omissions hide; labeled categories force each
  one to be addressed or explicitly marked empty.
- **Pinning the full conversation** — trying to pin everything as a
  substitute for summarizing, because that defeats the purpose of eviction
  and fills the context right back up with pins.
- **Leaving evict pins forever** — never unpinning the round summaries,
  because they are transition scaffolding, not ongoing reference; clean
  them up once the new context is stable.
