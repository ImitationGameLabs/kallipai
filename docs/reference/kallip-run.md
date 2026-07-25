# `kallip-run` Reference

Posts a prompt to a tagma agent and observes its run, streaming the agent's
procedure to stderr and exiting with a semantic exit code when the agent goes
idle (calls `break`) or hits a terminal error state. Designed for scripted and
automated workflows.

`kallip-run` is a **runtime-telemetry observer**: it does **not** print the
agent's message. A message is now a deliberate `kallip lesche send` CLI call the
agent addresses to the user over the relay/chat path — not a value on this
stream.
What `kallip-run` gives you is the procedure (reasoning, tool calls, results in
`--verbose`) and a machine-readable exit status.

By default the prompt goes to the tagma's **singleton root agent** (eagerly
created at tagma startup). Pass `--agent <ID>` to target a specific (sub)agent
instead — useful for running against a dedicated subagent when you need
isolation, since separate runs against the root share its context. The target
agent persists after the run.

```bash
kallip-run [OPTIONS] --prompt <PROMPT>
```

Uses `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_TAGMA_URL`
(env, default `http://127.0.0.1:3000`).

## Options

| Flag                | Description                                                    |
| ------------------- | -------------------------------------------------------------- |
| `--prompt <PROMPT>` | The prompt to send to the agent (required)                     |
| `--agent <ID>`      | Target an explicit agent by id instead of the tagma root       |
| `--json`            | Emit a single JSON object on stdout (see Output)               |
| `--verbose`         | Stream the agent's procedure (reasoning, tool calls) to stderr |

## Exit codes

| Code | Meaning                             |
| ---- | ----------------------------------- |
| 0    | Success (agent went idle / `break`) |
| 1    | Error                               |
| 2    | Max rounds exceeded                 |
| 3    | Cancelled                           |
| 4    | Token budget exceeded               |
| 5    | Failover chain exhausted            |

## Output

The output shape is driven by `--json` and `--verbose` (there is no TTY-based
auto-detection). The tagma already persists the agent's full execution history,
and the agent addresses the user via `kallip lesche send` (not this stream),
so the runner emits only a completion hint by default.

| `--json` | `--verbose` | stdout            | stderr                                                                                |
| -------- | ----------- | ----------------- | ------------------------------------------------------------------------------------- |
|          |             | _(nothing)_       | completion hint (agent id + how to continue)                                          |
|          | `--verbose` | _(nothing)_       | the procedure (`[reasoning]`/`[assistant]`/`[tool]`/`[tool-result]`/`[retry]`) + hint |
| `--json` |             | `{agentId, exit}` | diagnostics                                                                           |
| `--json` | `--verbose` | `{agentId, exit}` | procedure stream + diagnostics                                                        |

- No message is printed to stdout. The agent's bare assistant text is procedure
  only — in `--verbose` it streams to stderr prefixed `[assistant]`; it is not a
  user message.
- The JSON object **never contains `reasoning` or a user message**;
  `--verbose --json` streams the procedure to stderr but leaves the object
  unchanged.
- Warnings and errors always go to stderr.

`--json` example:

```json
{
  "agentId": "a3f1b2c4-5678-90ab-cdef-1234567890ab",
  "exit": "success"
}
```

`exit` is one of `success`, `error`, `max_rounds`, `cancelled`,
`budget_exceeded`, `failover_chain_exhausted`. If the tagma is unreachable, the
agent id is unknown, or `post_message` fails, no JSON object is emitted — the
error is printed to stderr and the exit code is `1`.

```bash
kallip-run --json --prompt "Refactor the config loader"
```

## Continuing a session

The target agent persists after the run. Its id is printed in the completion
hint, and you can continue the same session:

```text
$ kallip-run --prompt "Refactor the config loader"

agent a3f1b2c4-5678-90ab-cdef-1234567890ab went idle. Continue with: kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "<prompt>"
$ kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "and add a test"
```

Pass `--verbose` to watch the agent's reasoning and tool calls as it works:

```bash
kallip-run --verbose --prompt "Refactor the config loader"
```

A follow-up via `--agent` keeps the agent's full context. It works only against
a tagma that still has the agent registered — the same instance, or one that
restored it from disk on startup. `--agent` does not validate the id format; an
unknown id surfaces as a tagma error.

For the complete environment variable reference including LLM provider
configuration, see [env.md](env.md).
