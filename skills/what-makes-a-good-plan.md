---
name: What Makes a Good Plan
description: When you are writing or judging a plan and want the standard for what makes a plan good
---

# What Makes a Good Plan

A plan is worth acting on when it lets the user decide "is this the right thing
to do" before you spend the work. This is the standard for what makes a plan
good — consult it while planning. The workflow that produces one is `planning`.

## When to load

Consult this when you are writing or revising a plan. To run the planning
workflow, use `planning`; this skill only defines the standard.

## The core every plan shares

- **Framing — the why.** State the problem, need, or prompt that triggered the
  plan, because without it the user cannot judge whether the work is worth
  doing. This is the most important line; never omit it.
- **A precise task statement.** What "done" looks like.
- **One recommended approach.** A single path with its rationale, not a survey
  of alternatives — because listing options delegates the decision back. The one
  exception is a genuine fork whose tradeoffs are the user's call: present it
  crisply, and still recommend.
- **Reuse awareness.** Existing functions, patterns, or sources to build on,
  not reinvent.
- **Success criteria.** How you know it is done and right — a test, a command, a
  check.
- **Checkpoints.** Where you surface back to the user, especially before
  hard-to-reverse steps and when the plan must change.
- **Known unknowns.** The forks and uncertainties, resolved with the user or
  flagged.
- **Cost & risk.** The size of the change and where the blast radius falls,
  because the user needs that to judge whether to approve.

## Plan shapes

The core above is constant; each task type adds must-haves.

### Change plan

A code or config change. Must-haves: the touch-point files (with paths), the
reusable functions or patterns, and a verification step (what to run).

### Research plan

An open-ended search for an answer. Must-haves: a precise question, a source
strategy, and what counts as having an answer.

### Investigation plan

Debugging or root-causing. Must-haves: falsifiable hypotheses and how each is
tested.

### Design plan

An architectural decision, API shape, or schema. Must-haves: the decision to
make, the forces and constraints, one recommended option with its tradeoffs, and
what would reverse it.

## Avoid

- *Avoid:* a plan that dumps the "how" with no "why", because the user cannot
  judge value and may approve the wrong thing.
- *Avoid:* listing every approach instead of recommending one, because that
  hands the decision back to the user.
- *Avoid:* vague steps ("refactor the module") with no concrete touch points,
  because the plan is not executable.
