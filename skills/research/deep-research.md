---
name: Deep Research
description: When you need to investigate a topic deeply across multiple sources — classifying the domain and routing to the right specialized research flow
---

# Deep Research — the entry point

Going from "I need to understand X" to a synthesized answer grounded in
multiple sources. This skill owns the *shape* of research: frame the
question, plan the investigation (including parallelizable subtasks),
pick the right domain flow, execute it, then synthesize. The
domain-specific steps live in the domain flows it routes to.

## When to use

- You need to understand a topic deeply — not a quick lookup, but a
  systematic investigation across multiple sources
- You are comparing options, evaluating a landscape, or tracing a
  technical thread through primary sources

## When NOT to use

- A single `curl` or web search answers the question — that is a lookup,
  not research
- You already know the domain and want its specific flow directly — load
  `research/academic` or `research/open-source`

## The sequence

**Frame the question.** State what you are trying to understand and why.
A research question is not a keyword — it has a goal (evaluate, compare,
understand a mechanism, map a landscape) and a depth (survey vs.
deep-dive). Writing the question down forces specificity.
Done when:
- you can state the question in one or two sentences with a clear goal
- you know what a good answer would look like (a comparison? a summary?
  a recommendation?)

**Classify the domain.** The domain determines which sources matter and
how to access them. The two domains with dedicated flows today:
- **Academic / scientific** — papers, preprints, citations. Route to
  `research/academic`.
- **Open-source / technical** — repos, metadata, community discussion.
  Route to `research/open-source`.

If the question spans both (e.g., "evaluate MemGPT as a system"),
classify by the *primary* source type and supplement from the other.
If neither fits cleanly, use the closest flow as a template and adapt.
Done when:
- you have named the domain and the source types you will prioritize

**Plan the investigation.** Break the research into subtasks that can
run in parallel. This is where you decide what to delegate to subagents
and what to do yourself. The decomposition principle: split by source or
by question facet, not arbitrarily — each subtask should be
self-contained enough to produce findings independently.

Common decomposition patterns:
- **By source** — one subagent searches arXiv, another GitHub, another
  web search. Each returns findings; you merge.
- **By facet** — if the question has independent aspects (e.g., "compare
  X and Y" → one subagent investigates X, another Y), split by facet.
- **Mixed** — combine both: a source split within each facet.

Assign the domain skill (`research/academic`, `research/open-source`)
and source skills (`research/web-search`, `research/arxiv-reading`) as
`--skill` arguments to each subagent so they have the right workflow
loaded. The subagent prompt carries the scoped question and the
expected output format.

Create a research project directory at `/tmp/research/<short-name>/`
(kebab-case short name; with subdirs `papers/`, `repos/`, `notes/`) and
to each subagent in its prompt, so all artifacts land in one place.
Follow-up questions within the session can return to this directory
and find everything in place. These are session artifacts, not
deliverables — /tmp is ephemeral across reboots, so the directory
persists for the session but not beyond.
Done when:
- subtasks are defined and each has a clear scope, assigned sources, and
  expected output
- dependencies are identified (some subtasks may depend on results from
  others — e.g., citation tracing happens after initial paper discovery)
- parallelizable subtasks are marked and ready to dispatch
- the research directory `/tmp/research/<short-name>/` exists and its path
  is included in each subagent's prompt

**Execute.** Dispatch parallel subtasks first; while they run, do the
sequential work yourself or wait for results. Each subagent follows the
domain flow and reports findings. Merge results as they arrive.
Done when:
- all subtasks have reported or the deadline has passed
- you have raw findings from each source/facet

**Synthesize.** Combine findings into an answer that serves the
framed question. This is not a summary of what you read — it is an
argument or assessment grounded in the sources. Address the goal you
stated in step one: if the goal was comparison, produce a comparison;
if evaluation, produce a judgment with evidence.
Done when:
- the synthesis answers the framed question
- each claim is traceable to a source you examined

## Key behaviors to remember

- **Frame before you search** — jumping into search without a clear
  question wastes effort on irrelevant sources, because the question
  determines what counts as relevant.
- **Plan before you execute** — decomposing into parallel subtasks early
  maximizes throughput, because research is latency-bound (search,
  download, read all take time) and parallel subagents overlap that
  latency.
- **Split by source or facet, not arbitrarily** — each subtask needs a
  self-contained scope, because a subagent cannot coordinate with others
  mid-task; give it a question it can answer independently.
- **Synthesize, don't concatenate** — the output is an answer to the
  question, not a list of source summaries, because research without
  synthesis is just reading.

## Anti-patterns

- **Searching before framing** — starting with a vague keyword search,
  because without a framed question you cannot judge which results are
  relevant or when you have enough.
- **Treating research as search** — running queries and collecting links
  without reading and synthesizing, because collecting is not
  understanding.
- **Serializing parallelizable work** — running source searches one at
  a time when they are independent, because the total wall-clock time
  is the sum; parallel subagents make it the max.
