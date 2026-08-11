---
name: What Makes a Good Skill
description: When you are writing or revising a skill and want the standard for what makes a skill good
---

# What Makes a Good Skill

A skill has two faces. The **filename** is the programmatic id — the loading key
that `kallip skill meta` and cross-references key on (e.g. `skill/skill-management`);
pick it once and keep it stable. The **frontmatter `name` + `description`** are the
readable face — the single entry `kallip skill index` shows. The `name` is the
reading-friendly label (think of it as the first half of the entry); the
`description` is the match trigger, the only thing read at index time to decide
whether to load. Frontmatter is exactly those two fields.

This skill is the craft of writing the *content* — the readable face and the body.
For whether a skill is worth creating, and how the system discovers and organizes
them, see `skill/skill-management`.

## When to load this skill

Consult this when you are writing or revising a skill's content — the description,
the body, the voice. For the end-to-end workflow to create one from scratch, use
`skill/creating-a-skill`; to review a skill against this standard, use
`skill/reviewing-a-skill`; for deciding *whether* a skill is worth creating or for
locating/organizing skills, use `skill/skill-management`.

## The description is the match trigger

The description is the most important line you write. At index time the agent sees
only `name` + `description` — never the body — and decides whether to load from
that alone. A description with no trigger is invisible at the moment of decision,
because there is nothing to match against the task at hand. It is a **trigger
condition** — when to reach for the skill — not a summary of its internal steps,
because the index matches on the situation the agent is in, and a step-summary
goes stale whenever the body changes.

Done when:

- it opens with a trigger — `When`, `How to`, or for a prerequisite skill, the prerequisite imperative itself (e.g., "Load X before Y") — naming the task shape;
- it states when to reach for the skill (the triggering situation), not a summary of its internal steps;
- it names the task the agent faces, not just the topic;
- if the skill has a required prerequisite — a guide to load, a command to run, a state to check — the description embeds that imperative into the trigger, because a trigger that omits the prerequisite gets matched but the first step gets skipped;
- it is one line.

Weak: "Guidance about editing files." — no trigger; an agent facing "modify this
config" finds nothing to match. Strong: "When to use aifed and how to integrate it
with agent context management for text editing and coding tasks" — names the task
shape (and the tool beside it).

*Avoid:* a topic-only label ("About context") with no task trigger, because nothing
matches; pure mechanism ("Skill file format") with no task shape, because the agent
searches by what it is trying to do; a step-summary ("How to X — do A, then B, then
C") in place of a trigger, because the index matches on when to use the skill, not
what is inside it, and the summary drifts when the body changes.

## Writing the readable entry: name + description together

Treat `name` + `description` as one unit — the single line the index shows. Write
them together so they read as one entry: the `name` is the reading-friendly label
(Title Case; may differ from the filename), the `description` completes it with the
trigger. A `name` that just restates the filename adds nothing readable.

The **filename** is the programmatic id from the opening — keep it kebab-case,
stable, and domain-not-tool (`testing`, not `cargo-test`). For the mechanical routing
rules (kebab-case paths, path-as-id, category placement, two-level depth), see
`skill/skill-management`.

*Avoid:* a tool-named filename (`cargo-test`), because agents route by domain, not
by binary; a `name` that merely echoes the filename, because it wastes the reading
surface.

## Two archetypes: Process or Reference

Most skills are one of two shapes; the body structure follows from which.

- **Process** — a sequence the agent runs in order. Reach for it when order matters
  and a skipped step fails: "first build a feedback loop, then hypothesize, then
  verify." The value is doing the steps in the right order.
- **Reference** — vocabulary and criteria the agent consults mid-work. Reach for it
  when the value is disambiguation: "X counts as Y when…; X is not Y when…." The
  value is precision, not sequence.

Decision rule: if you can state the skill as an ordered procedure → Process, because
order is load-bearing. If you can state it as distinctions and definitions →
Reference, because disambiguation is load-bearing.

A third, thinner shape is the **Wrapper** — a skill that points at a `--reference`
or `--skill` command emitting the full reference and adds only when/why (template
below). It has no gates, because it has no sequence.

A Process skill may carry a small reference block (e.g. `## Key Behaviors to
Remember`), but the spine is one archetype. The reverse is fine too: a `Done when:`
gate inside a Reference skill is a check, not a sequence marker.

## A recurring pattern: workflow + standard pair

When a domain has both a recurring activity and a body of criteria, split it into
a **Process workflow** + a **Reference standard** that delegate to each other: the
workflow owns the ordered steps, the standard owns the criteria, and the workflow
checks against the standard instead of restating it. Reach for this pair when one
skill would otherwise mix "how to run the steps" with "what good looks like."
Examples: `creating-a-skill` (Process) + `what-makes-a-good-skill` (Reference);
`planning` (Process) + `what-makes-a-good-plan` (Reference).

## A recurring pattern: the review-fix loop

A Process skill with a review step is a fix-re-review loop: the review requests
changes, the author fixes, and the fix is re-reviewed. That step should hand the
reader the loop's termination and convergence — defined in
`skill/reviewing-a-skill` — not a bare "re-run until it passes".

*Avoid:* a review step that says only "re-run until it passes", because it hides
the termination exits and the oscillation guard, leaving the loop to end by giving
up; point at the loop rules instead.

The templates below are starting points for a domain skill, each preceded by the
`name` + `description` frontmatter from above. A meta-skill that spans a whole topic
— this one included — won't pin cleanly to a single template; most skills do.

## Process template

```markdown
# <Name> — <gerund or noun>

<1-2 sentences: the task shape that should load this, and the goal.>

## When to use

- <triggering condition>
- <triggering condition>

## When NOT to use

- <condition>, because <reason> — <what to do instead>

## The sequence

**<Step name>.** <What to do and why, in reasoned voice — "X because Z".>
Done when:
- <checkable yes/no criterion>
- <checkable yes/no criterion>

**<Step name>.** <...>
Done when:
- <...>

## Key behaviors to remember

- **<bold lead>** — <reasoned, with the condition under which it holds>.

## Anti-patterns

- **<pattern>** — <why it fails, because reason>; <reasoned alternative>.
```

The load-bearing detail is the **`Done when:` gate**. Add one wherever a step's end
is not obvious from the prose, because without it the steps blur and the agent has
no check for when one step is finished and the next can start. Every criterion must
be checkable — a test the agent can run against its own output.

*Avoid:* a gate that merely restates the step ("read the issue" → "Done when: you have read the issue"), because it adds noise without a checkable boundary; use a gate when, without it, the agent would plausibly advance before the step is actually done.

Name each step; do not number them. Order is document order (top to bottom), so a
number adds nothing — and dropping it means inserting or deleting a step never
cascades a renumber, and the diff stays clean. The same logic applies to `Key
behaviors to remember` and `Anti-patterns`: use bullets, not a numbered list,
because the items are parallel and the ordinal carries no information. Reserve
numbers for the rare case where the ordinal itself is load-bearing.

## Reference template

```markdown
# <Name> — Reference

<1-2 sentences: what this disambiguates, and why common sense is not enough.
If the agent would already get this right, the skill has no value.>

## <Core concept>

<Definition, leading with what makes it non-obvious.>
*Avoid:* <rejected framing> — because <reason>; use <correct framing> when <condition>.

## Decision rules

- If <condition>, then <conclusion>, because <reason>.

## Anti-patterns

- **<misuse>** — <why it is wrong, because reason>; <reasoned alternative>.
```

The core device is the **`*Avoid:` block**. A Reference skill's whole value is
disambiguation, so the framings you reject carry as much weight as the definitions
you assert — but each needs its `because`, or it reads as doctrine. A Reference
skill earns its pinned-context cost by carrying expertise the agent does not already
have; if a section only restates what `--help` or common sense supplies, cut it and
link the source instead.

## The Wrapper shape

When a tool ships its own complete, always-current reference, do not duplicate it.
Write a thin skill that points at the command and adds only what the docs cannot:
when to reach for it, and the semantics the flag list cannot express.

```markdown
# <Name>

<One paragraph: when to reach for this, and what it is for.>

## Getting Started

<the command that emits the full reference, e.g. `aifed --skill` or `kallip --reference`>

## Semantics to remember

- **<bold lead>** — <a behavior the docs cannot express>.
```

`aifed` and `agent/kallip` are the in-repo examples (`aifed` titles the section "Key
Behaviors to Remember"; `agent/kallip` titles it "Semantics to remember"). Both use
the bullets-not-numbers style shown above. There are no gates — there is no sequence.

Any skill with a required prerequisite should embed that imperative into the description trigger (e.g., "Load this skill before running any agent-browser command"), because a skill whose first step gets skipped is matched but useless. This applies whenever the skill's value depends on an action the agent must take before using it. See the Done-when criterion above.

## Reasoned voice: gates and constraints without doctrine

kallipai skills are reasoned, not imperative: bare "always X / never Y" reads as
doctrine and gets followed blindly, because it invites obedience instead of
judgment. The two devices above — gates and negative constraints — fit that voice
when framed right.

**Gates are checks, not commands.** A `Done when:` is something the agent verifies
about its own output, not something it obeys, so it is already reasoned. Its failure
mode is being phrased as an order ("Always include a trigger verb") that loses the
checkable yes/no quality. Keep gates phrased as tests.

**Negative constraints need a because.** Default to `Avoid X when <condition>,
because <reason>; prefer W`. Reserve the bare `Never X` for the narrow case where
blind obedience is safer than judgment — a concrete, near-irreversible failure — and
even then keep a compact `because` inline, as `aifed` does ("Never mix tools … it
breaks hash verification on both sides").

| Doctrinal (avoid) | Reasoned (prefer) |
| --- | --- |
| "Never name a skill after the tool." | "A tool-named id (`cargo-test`) routes poorly because agents search by domain, not binary; name the id after the domain (`testing`)." |
| "Always put a trigger verb in the description." | "A description with no trigger verb is invisible at index time, because nothing matches the task — lead with the task shape." |
| "Every step needs a Done-when." | "A step without a gate blurs into the next, because there is no check for when to advance — state the boundary as a checkable criterion." |
| "Never mix aifed with sed." | (footgun — the bare `Never` is preferred here, with a compact `because` kept inline) "Never mix aifed with `sed`/`cat` — it breaks hash verification on both sides." |

Row 4 is the documented exception: for a genuine footgun the bare `Never` is the
preferred form. Rule of thumb elsewhere — default to reasoned; reach for a bare
prohibition only when blind obedience is safer than judgment.

## Anti-patterns

- **Writing the body before the description** — the description decides whether the
  body is ever read; write and pressure-test it first, because a great body behind a
  triggerless description is never loaded.
- **A Process skill with no gates** — the steps collapse into prose, because there is
  no check for when one ends; add a `Done when:` per step.
- **A Reference skill that restates common sense** — if the agent would already get
  it right, the skill adds pinned-context cost for no value, because the point of a
  Reference skill is expertise beyond common sense; cut the section or link the source.
- **Doctrine where judgment should weigh** — bare "always/never" gets followed
  blindly, because it reads as a rule rather than reasoning; prefer the `because`
  form, except for genuine footguns.
