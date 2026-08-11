---
name: Commit Messages
description: When you are writing or reviewing a git commit message — the format, body density, and type/scope conventions that make a message useful rather than noise
---

# Commit Messages — Reference

A commit message tells a reviewer what changed and why, at a glance. The
format is Conventional Commits (`type(scope): subject` + body), but the
hard part is not the format — it is body density: what to include and what
to leave to the diff. This skill defines the standard for message quality;
for the full commit workflow (branch check, staging, drafting, review),
see `code/committing`.

## Format

```
type(scope): subject          ← subject line (imperative, lowercase, no period)

Body paragraph or bullets     ← what changed and why (omit for trivial changes)

BREAKING CHANGE: description  ← footer (only for breaking changes)
```

The subject is the only line most people read, so it carries the message.
The body explains the *why* the subject cannot. The footer marks breaking
changes for tooling that parses it.

## Choosing the type

The type names the category of change, not the file it touches.

- **feat** — new functionality (a user or API can do something new).
- **fix** — a bug repair (something that should have worked now does).
- **refactor** — restructuring without behavior change. Reach for this
  when the diff is structural — renaming, splitting modules, extracting
  abstractions — because calling it `feat` or `fix` misleads anyone
  scanning the log for behavior changes.
- **docs** — documentation only (README, doc comments, guides).
- **chore** — maintenance that is neither code behavior nor docs —
  dependency bumps, formatting sweeps, config noise.
- **build** — build system, packaging, CI infrastructure.
- **style** — formatting, whitespace, no logic change.
- **test** — adding or fixing tests.
- **perf** — performance improvement without behavior change.

## Choosing the scope

The scope names the module or crate the change lives in — not the
mechanism. A change to `kallip-tagma`'s relay module is
`feat(tagma):` or `fix(relay):`, not `feat(tokio):` just because it
touches async code. Omit the scope when the change is cross-cutting
(a project-wide rename, a repo-level docs pass), because a scope that
spans the whole project adds no information.

*Avoid:* inventing a scope for every commit, because a scope that
merely echoes the type (`chore(misc):`) or names a single file adds
noise without routing value; omit it when no module-level scope fits.

## Subject line quality

- **Imperative mood** — "add", "fix", "drop", "rename" — not "added"
  or "adds", because the subject completes the sentence "if applied,
  this commit will ___".
- **Lowercase after the colon** — conventional in this project's
  history, because consistency makes the log scannable.
- **No trailing period** — the subject is a title, not a sentence.
- **Specific but concise** — "fix: include listen address in bind
  errors" is good; "fix: bug" is useless; "fix: wrap TcpListener::bind
  with anyhow with_context reporting args.listen_addr in four service
  mains" is implementation detail that belongs in the body.

## Body density: the core judgment

The body exists to answer three things at the level a reviewer
needs: what changed, why the change was needed, and why the approach
works — not to narrate the diff. The diff already exists; repeating it
in prose adds length without adding understanding.

**Single-concern change:** one short paragraph stating what changed,
why it was needed, and why the approach works. No bullets needed,
because the change is one thought.

```
fix(ui): preserve single newlines in agent markdown

Enable GFM line breaks in the markdown renderer so single newlines
become <br> instead of collapsing to spaces, which flattened agent
output like directory trees into one line.
```

The strongest bodies also say *why the approach works*, not just why it
was needed — e.g. "read_timeout resets on every frame received, so only
genuine silence trips it" — because that is the reasoning a future
reader cannot reconstruct from the diff alone.

**Multi-concern change:** a one-line lead summarizing the whole change,
then one bullet per concern. Each bullet is one or two sentences —
enough to convey the what and why of that concern, not its
implementation.

```
feat(rooms): add multi-member room chat

- Multi-member rooms: create, invite, open-access join, and chat
  with per-member presence and room history.
- Public profiles: user and tagma profile pages backed by new
  agora endpoints.
```

*Avoid:* packing each bullet with implementation detail (function
names, SQL columns, wire field names, type signatures), because the
reader who needs that level can read the diff — the body should
convey the *shape* of the change, and a dense bullet that reads like
a code review comment loses the forest for the trees.

*Avoid:* writing a design document in the body (section headers, nested
bullets, paragraphs of rationale), because a commit message that
outgrows ~15 lines stops being scannable; if the change needs that
much explanation, link to a doc or issue instead.

## Breaking changes

Mark a breaking change with `!` after the type/scope and a
`BREAKING CHANGE:` footer, because tooling (and readers) need to spot
these without reading every body.

```
refactor(tagma)!: rename kallip-daemon to kallip-tagma

<body>

BREAKING CHANGE: the crate, binary, env vars, and CLI flags rename.
Operators must update KALLIP_DAEMON_ADDR to KALLIP_TAGMA_ADDR.
```

The footer should name *what breaks for the consumer* — env vars,
API shapes, CLI flags — not just "things changed", because the
consumer needs to know what to update.

## Decision rules

- If the change adds user-visible functionality, then `feat`, because
  the log should reflect capability growth.
- If the change restructures code without changing behavior, then
  `refactor`, because `feat` or `fix` misleads log scanners.
- If the body would exceed ~15 lines, then it is trying to do too
  much — split the commit or cut to the essential why, because a
  message that long stops being read.
- If the change breaks an API, env var, or CLI contract, then mark
  it with `!` and a `BREAKING CHANGE:` footer, because that is what
  downstream consumers need to find.
- If the change is trivial (formatting, a one-line doc fix), then
  the subject alone suffices, because a body restating the subject
  adds noise.

## Anti-patterns

- **Implementation-detail body** — narrating every function renamed
  and every field changed, because the diff already records this and
  the body should convey the shape of the change, not reconstruct it.
- **Wall-of-text body** — multiple unbroken paragraphs with no
  bullets when there are several concerns, because the reader cannot
  skim a wall of prose; use one bullet per concern.
- **Subject too vague** — "fix: various bugs" or "refactor: cleanup",
  because a subject that could describe any commit describes none.
- **Missing body on a non-trivial change** — a `feat` or `refactor`
  with a subject but no explanation of why, because the subject alone
  rarely conveys the reasoning behind a structural change.
- **Scope as file name** — `fix(src/main.rs):` instead of
  `fix(tagma):`, because the scope should route by module, not by
  path; a reader scanning the log thinks in domains, not file paths.
- **Process metadata in the body** — "reviewed by", "tested by",
  "approved by" belong to the PR or review system, not the commit
  message, because the body answers what changed and why, not who
  signed off on it; a reader scanning the log wants the change shape,
  not the process trail.
