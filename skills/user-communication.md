---
name: User Communication
description: When you need to send a message to the operator — use lesche for status updates, questions, reports, and any response the operator should see
---

# User Communication — reaching the operator via lesche

The operator is not in your context window — they read your messages
through the lesche relay. A plain response with no tool call does not
reach them; it triggers a heartbeat that re-prompts you. Every message
the operator should see must go through `kallip lesche send`.

For the full lesche command syntax (rooms, read, flags), run
`kallip --reference` — this skill covers the when/why and semantics; the
reference covers the complete flag list.

## When to use

- You have a status update, question, or report for the operator
- You completed a task and want to confirm the outcome
- You need a decision from the operator before proceeding

## When NOT to use

- Messaging a subagent or peer — use `kallip message <ID> <TEXT>` instead
- Replying in a multi-member room — use `kallip lesche send --room <ROOM>`
  (copy the room id from the `[From: ... | room <id>]` header)
- Internal reasoning that the operator does not need to see

## Sending a message

```bash
kallip lesche send "<message>"
```

For multiline messages, pipe via stdin to avoid shell escaping issues:

```bash
cat <<'MSG' | kallip lesche send
Your multiline message here.
MSG
```

Use the heredoc form whenever the message contains backticks, quotes,
or special characters, because inline text with those characters gets
mangled by the shell.

## Semantics to remember

- **Every response to the operator goes through lesche.** A response
  without `kallip lesche send` vanishes — the operator never sees it,
  because plain text triggers a heartbeat, not a message delivery. This
  is the single most common communication failure.
- **`lesche send` without `--room` is the bilateral 1:1.** This is the
  default channel for direct operator communication. Use `--room` only
  when replying in a multi-member room, copying the room id verbatim.
- **Fire-and-forget.** `lesche send` returns immediately; you will not
  get a read receipt. Do not poll for delivery — the operator's next
  message confirms receipt.
- **Read room history with `lesche read`.** If you rejoin a conversation
  or lose context, `kallip lesche read --room <ROOM>` pulls recent
  history so you can reconstruct what was said.

## Anti-patterns

- **Answering without sending** — writing a response but not calling
  `kallip lesche send`, because the operator never receives plain text;
  the harness re-prompts you instead of delivering the message.
- **Inline text with special characters** — backticks and quotes in
  inline `lesche send "..."` get eaten by the shell, because the shell
  interprets them before the command runs; use the heredoc form.
