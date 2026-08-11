---
name: What to Preserve
description: When you are writing an eviction summary or any context-preserving summary — the dimensions that must survive and the quality criteria that distinguish a summary you can work from after eviction from one that loses the thread
---

# What to Preserve — Reference

An eviction summary is the only thing that survives the transition
between contexts, so it carries a burden no ordinary summary does:
it must be sufficient to continue the work without the turns it
replaces. The failure mode is not a poorly written summary — it is a
summary that looks complete but drops a dimension that matters later,
like a decision's rationale or a file locator. This Reference defines
what must be in the summary and what separates a reliable summary
from a plausible-looking one.

## The dimensions

Every eviction summary must address each of these dimensions.
"Address" means either filling it or explicitly marking it empty —
a dimension omitted entirely is indistinguishable from one forgotten,
because silence and omission look the same after eviction.

- **Current task and goal** — what you are doing, why, and how far it
  has gotten. The first thing the post-eviction context needs, because
  without it nothing else has context.
- **Decisions and their rationale** — not just what was decided, but
  *why*. The rationale is more valuable than the conclusion, because
  after eviction you need the reasoning to judge whether the decision
  still holds — a conclusion without a reason is a rule you cannot
  reassess.
- **Current state and progress** — what is done, what is in progress,
  what is next. The resumption point, because a summary that does not
  say where you are forces a full re-read to reconstruct it.
- **Locators** — file paths, commit hashes, agent IDs, plan paths, room
  IDs. Concrete references the post-eviction context will need, because
  "the runtime crate" is not as useful as
  `crates/kallip-runtime/src/tools/skill/mod.rs`.
- **User constraints** — preferences, limits, and requirements the user
  stated. These constrain future work and are easy to lose, because
  they often appear as asides rather than primary instructions.
- **Rejected approaches** — paths considered and discarded, with the
  reason for rejection. Prevents re-proposing a dead end, because
  without this record the post-eviction context has no memory of why
  something was already ruled out.
- **Open blockers** — anything stuck or waiting. If a blocker exists
  and is not recorded, the post-eviction context will rediscover it
  the hard way.

*Avoid:* treating dimensions as a checklist you fill from memory,
because memory is exactly what eviction removes; scan the actual turns
for each dimension before summarizing.

*Avoid:* omitting a dimension because it seems empty, because an
omitted dimension is indistinguishable from a forgotten one — state
"No blockers" or "No user constraints" explicitly.

## Quality criteria

The dimensions define *what* to include. These criteria define *how*
to write each entry so the summary is usable after eviction.

- **Structured, not prose.** Label each dimension with a header or tag
  so every one is visible and accountable. A wall of prose lets
  omissions hide, because the reader cannot tell which dimension was
  skipped; a labeled section forces each to be addressed or marked
  empty.
- **Rationale over conclusion.** "Decided to squash commits" is weaker
  than "Decided to squash because the intermediate commits were
  incremental tweaks to one coherent change." The rationale lets a
  future context judge whether the decision still applies.
- **Concrete references.** File paths, not "that file." Commit hashes,
  not "the recent commit." Agent IDs, not "the reviewer." A locator
  the post-eviction context can act on directly, because a vague
  reference forces a search that may fail.
- **Compression over transcript.** Compress to the essential information
  per dimension — intermediate tool outputs, trial-and-error sequences,
  and process narration belong to the turns being evicted, not the
  summary, because the goal is enough to continue, not a record of what
  happened.
- **Explicit emptiness.** "No blockers" or "No user constraints" is
  more valuable than silence, because an empty marker is a conscious
  decision that the dimension was considered, while silence could be
  an oversight.

## Decision rules

The criteria above carry their own because-clauses and anti-pattern
implications inline, so the rules below cover only the points the
criteria do not already make.

- If a dimension has no content, then mark it empty explicitly, because
  omission and forgetfulness look identical after eviction.

- If a locator appears in the turns, then preserve it verbatim (path,
  hash, ID), because paraphrasing a locator defeats its purpose.

## Cross-reference

For a multi-round process that applies these criteria under pressure —
scanning, gap-checking, and pinning each round before evicting — see
`preserving-context`.
