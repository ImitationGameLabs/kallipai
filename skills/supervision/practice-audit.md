---
name: Practice Audit
description: When you receive a checkpoint report from the root agent — check work practices against the skill library for three drift types and produce advisory findings
---

# Practice Audit — checking behavioral drift against standards

The root agent reports at checkpoints; your job is to detect behavioral
drift — places where the root agent's actual practice diverged from what
the skill library prescribes. You check three drift types, produce
advisory findings, and send them back. You do not enforce — the root
agent decides whether to act.

## When to use

- You received a checkpoint report from the root agent
- The root agent asked you to audit recent work

## When NOT to use

- To review skill content quality — that is `skill/reviewing-a-skill`
- To review code quality — that is a code review, not a practice audit
- To review a plan — that is `planning`'s review step

## The audit

**Load the skill index.** Run `kallip skill index <skills-path>` to get
the current skill catalog, because every drift check references the
index to determine which skills should have been loaded.
Done when:

- the skill index is in your context

**Check for skill-not-loaded drift.** For each task in the checkpoint
report, scan the skill index for matching descriptions. If a skill
matches the task shape but the report does not list it as loaded, flag
it.
Done when:

- every task in the report has been checked against the index, and each match not listed as loaded is flagged

**Check for process-skipped drift.** For each skill the report says was
loaded, verify that its process steps were completed. The report should
name which steps ran and which were skipped. If a skill was loaded but
its steps are not reported as completed, flag the gap.
Done when:

- each loaded skill's process steps are accounted for, and any unreported or skipped steps are flagged

**Check for artifact-standard drift.** Read `git log` and `git diff`
for the checkpoint's commits. Check commit messages against
`code/commit-messages`, skill files against `skill/what-makes-a-good-skill`,
and any other artifact against its applicable standard.
Done when:

- every artifact has been checked against its standard, and deviations are flagged

**Produce findings.** For each drift, name the type, the task or
artifact, the expected standard, and the observed deviation. Send the
findings to the root agent via `kallip message`. If no drift is found,
send a clean confirmation — silence is ambiguous.
Done when:

- findings are sent, or a clean confirmation is sent

## The three drift types

These are the behavioral patterns worth checking, because they are the
ones that recur and that the root agent does not self-correct:

- **Skill-not-loaded** — a task matched a skill description at index
  time, but the skill was not loaded, because the root agent did not
  consult the index before acting. This is the most common drift.
- **Process-skipped** — a skill was loaded but its process steps were
  not completed, because loading a skill without following its sequence
  gives a false sense of compliance.
- **Artifact-standard** — a concrete artifact (commit message, skill
  body, file change) deviates from its applicable standard, because
  the standard exists but was not checked against the output.

## Key behaviors to remember

- **Check the index, not your memory.** Run `kallip skill index` each
  audit cycle, because the skill library changes between cycles and a
  stale mental model misses new skills.
- **Flag specific tasks and artifacts.** "You forgot to load a skill"
  is useless; "task X matched `code/committing` but it was not loaded,
  and the commit message lacks a body" is actionable, because the root
  agent needs the specific mapping to remediate.
- **Send findings even when clean.** A clean confirmation tells the root
  agent the audit ran and found nothing, because silence is
  indistinguishable from "the audit did not run."

## Anti-patterns

- **Auditing without the index** — checking from memory, because the
  skill library grows and a memorized index drifts from reality.
- **Vague findings** — "you should load more skills", because the root
  agent cannot act on vagueness; name the skill, the task, and the gap.
- **Enforcing instead of advising** — treating findings as commands,
  because the auditor lacks the root agent's task context and may flag
  a skill that genuinely did not apply; advise, let the root agent weigh.
