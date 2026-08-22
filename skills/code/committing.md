---
name: Committing
description: When you have changes ready to commit — the ordered workflow from branch check through staging hygiene, message drafting, and review to the final commit
---

# Committing — from changes to a reviewed commit

Going from "I have changes" to a clean commit involves safety checks that
prevent the mistakes an agent is most prone to: committing to the wrong
branch, staging files that should be ignored, and writing a message
without review. This skill owns the ordered sequence; the standard for
message quality lives in `code/commit-messages`.

## When to use

- You have changes in the working tree and are about to commit them

## When NOT to use

- To look up what makes a good commit message (format, body density,
  type/scope), because that is `code/commit-messages`; this skill runs
  the sequence and delegates message quality there.
- To amend or rebase existing commits, because this skill covers the
  forward commit path only.

## The sequence

**Confirm the branch.** Check `git branch --show-current` before
staging anything. Committing to `main` when you meant a feature
branch, or vice versa, is the single most common commit mistake — and
it is invisible until someone notices the commit in the wrong place.
Done when:

- you have confirmed the branch name and it matches your intent

**Review the working tree.** Run `git status` and scan every entry
before staging. Two things to catch here: files that should be
gitignored but are not yet (build output, `.env` files, editor
artifacts, large binary blobs), and changes that belong in a separate
commit. A blanket `git add -A` skips both checks — it sweeps
everything in, including files you did not mean to include.

If you spot a file that should be ignored, add it to `.gitignore`
first (and commit the `.gitignore` change if it is new), because once
a file is tracked, removing it from history is costly.

Done when:

- every entry in `git status` has been reviewed
- files that should be ignored are caught (staged separately or
  gitignored)
- you know which changes belong in this commit and which do not

**Stage selectively.** Stage the files for this commit — individually
(`git add <file>`) or by group — rather than `git add -A`, so that
unrelated changes stay out. If a logical change spans files across the
tree, stage exactly those files together; if the working tree has
multiple independent concerns, they are separate commits.

Done when:

- `git diff --cached` shows exactly the changes you intend to commit
- no unintended or unrelated files are staged

**Draft the commit message in a temp file.** Write the message to
`/tmp/commit-msg.txt` (or a similar temp path), following the standard
in `code/commit-messages`. Drafting in a file separates writing from
committing: you can read the staged diff and the message side by side,
edit freely, and let the message settle before it becomes history. Use
`git commit -F /tmp/commit-msg.txt` when you are ready.

Done when:

- a commit message is written to a temp file
- it follows the format and body-density standard in
  `code/commit-messages`

**Review the staged content and the message.** Read `git diff --cached`
and the drafted message together. Check two things: that the staged
diff matches what the message claims (no stray debug prints, no
accidental formatting churn), and that the message accurately captures
the change at the right altitude per `code/commit-messages`. This step
can be self-review or delegated to a subagent for an independent read.

Done when:

- the staged diff is clean (no stray changes, no debug artifacts)
- the message matches the diff and meets the standard in
  `code/commit-messages`
- review feedback has been considered

**Absorb feedback and refine.** If the review surfaced a problem — a
stray file, a misleading subject, an over-detailed body — fix it: adjust
staging, rewrite the message, or split the commit. Re-read the diff and
message after each fix, because a fix can introduce a new mismatch.
Not every review note needs to be adopted; weigh each one against the
standard and your knowledge of the change.

Done when:

- each review finding is either addressed or consciously accepted with
  a reason

**Commit.** Run `git commit -F /tmp/commit-msg.txt` with the reviewed
message. Verify the commit landed correctly with `git log --oneline -1`
and `git show --stat HEAD`, because a commit that silently failed or
included the wrong files is not caught until later.

Done when:

- the commit is created and verified (`git log` shows it with the
  expected message and file set)

## Key behaviors to remember

- **Branch before staging** — confirming the branch takes one command
  and prevents the most disruptive commit mistake, because undoing a
  commit on the wrong branch means a reset plus a re-commit or a
  cherry-pick.
- **Review the tree before `git add`** — a blind `git add -A` includes
  everything in the working tree, including files that should be
  ignored or changes that belong in a separate commit, because the
  staging step has no judgment without the review step.
- **Draft in a file, not inline** — `git commit -m "..."` locks in the
  message before you can review it against the diff, because inline
  drafting skips the review step entirely; writing to `/tmp` and using
  `-F` keeps the message editable until you commit.
- **Review the diff and message together** — the message must match
  the diff, and only reading them side by side catches mismatches,
  because a message written from memory can drift from what was
  actually staged.

## Anti-patterns

- **`git add -A` without reviewing** — staging everything blindly,
  because this includes gitignore misses and unrelated changes; review
  `git status` first, then stage selectively.
- **Inline message, no review** — `git commit -m "fix stuff"`, because
  a one-liner written in the shell skips the drafting and review steps
  that produce a useful message; draft in a file and review.
- **Committing without checking the branch** — staging and committing
  on whatever branch happens to be checked out, because the commit
  lands in history on that branch and moving it later is error-prone.
- **Skipping the post-commit verification** — assuming the commit
  succeeded without checking, because a pre-commit hook rejection or a
  partial staging failure can leave the working tree in an unexpected
  state.
