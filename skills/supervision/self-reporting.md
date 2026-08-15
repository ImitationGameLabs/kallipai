---
name: Self-Reporting
description: When you complete a task checkpoint (a commit, a review, or a milestone) — send a structured work report to the auditor subagent so it can check for behavioral drift
---

# Self-Reporting — checkpoint reports to the auditor

An auditor subagent checks your work practices against the skill
library. It cannot read your context — it only sees what you report
and what git records. This skill defines when to report, what to
include, and how to handle the auditor's feedback.

## When to use

- You completed a task and committed it (the primary checkpoint)
- You finished a multi-step process (review, research, skill creation)
- You received a correction from the operator and resolved it

## When NOT to use

- Mid-task — wait for a natural stopping point, because an incomplete
  task gives the auditor nothing actionable
- For trivial one-liners that need no skill and no process — the auditor
  can detect these from git history without a report

## The checkpoint report

Send the report to the auditor subagent via `kallip message`:

```bash
kallip message <auditor-id> <<'REPORT'
CHECKPOINT REPORT
Tasks: <what you did>
Skills loaded: <which skills you loaded and followed, or 'none'>
Process: <which process skills you ran, and whether you completed all steps>
Artifacts: <commits, files changed, reviews completed>
REPORT
```

Done when:

- the report names each task and the skills loaded for it (or states none were needed)
- the report states whether each process skill's steps were completed in full
- the report lists the concrete artifacts (commits, files) the auditor can verify

## Handling auditor feedback

The auditor sends advisory findings — you decide whether to act.

- **Skill-not-loaded finding** — load the named skill now and check
  whether its process applies to work already done; if it does and a
  step was missed, remediate (run the step, re-commit, re-review).
- **Process-skipped finding** — identify which step was skipped and
  whether it is still recoverable; if the artifact already shipped with
  the gap, note it for the next cycle.
- **Artifact-standard finding** — fix the artifact (amend the commit,
  update the skill body) and confirm the fix.
- **Accept-with-reason** — if the finding does not apply (the task
  genuinely did not need that skill), record the reason and move on.

Done when:

- each finding is either remediated or accepted with a recorded reason

## Key behaviors to remember

- **Report honestly, including omissions.** The auditor's value depends
  on accurate reporting; hiding a skipped step defeats the purpose,
  because the auditor cannot flag what it cannot see.
- **Report at checkpoints, not mid-task.** A checkpoint is a commit, a
  review completion, or a task milestone, because the auditor needs
  finished artifacts to verify.
- **Treat findings as advisory.** The auditor may misjudge context you
  have but it does not; weigh each finding against your knowledge of the
  task, because blind compliance with an incorrect finding wastes effort.

## Anti-patterns

- **Reporting only successes** — omitting skipped skills or incomplete
  steps, because the auditor is designed to catch drift, not to be
  impressed; a report that hides gaps makes the audit worthless.
- **Waiting too long to report** — batching many tasks into one report,
  because the auditor loses granularity and cannot map findings to
  specific tasks; report per checkpoint.
