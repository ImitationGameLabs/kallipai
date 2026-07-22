# `kallip-run` Reference

Posts a prompt to a daemon agent and prints the final assistant reply to stdout,
exiting with a semantic exit code. Designed for scripted and automated workflows
where the caller needs machine-readable output and exit-status-driven control
flow.

By default the prompt goes to the daemon's **singleton root agent** (eagerly
created at daemon startup). Pass `--agent <ID>` to target a specific (sub)agent
instead — useful for running against a dedicated subagent when you need
isolation, since separate runs against the root share its context. The target
agent persists after the run.

```bash
kallip-run [OPTIONS] --prompt <PROMPT>
```

Uses `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_DAEMON_URL`
(env, default `http://127.0.0.1:3000`).

## Options

| Flag                | Description                                                    |
| ------------------- | -------------------------------------------------------------- |
| `--prompt <PROMPT>` | The prompt to send to the agent (required)                     |
| `--agent <ID>`      | Target an explicit agent by id instead of the daemon root      |
| `--json`            | Emit a single JSON object on stdout (see Output)               |
| `--verbose`         | Stream the agent's procedure (reasoning, tool calls) to stderr |

## Exit codes

| Code | Meaning                  |
| ---- | ------------------------ |
| 0    | Success                  |
| 1    | Error                    |
| 2    | Max rounds exceeded      |
| 3    | Cancelled                |
| 4    | Token budget exceeded    |
| 5    | Failover chain exhausted |

## Output

The output shape is driven by `--json` and `--verbose` (there is no TTY-based
auto-detection). The daemon already persists the agent's full execution
history, so by default the runner is **minimal**: just the final reply.

| `--json` | `--verbose` | stdout                       | stderr                                                                  |
| -------- | ----------- | ---------------------------- | ----------------------------------------------------------------------- |
|          |             | final assistant reply        | completion hint (agent id + how to continue)                            |
|          | `--verbose` | final assistant reply        | the procedure (`[reasoning]`/`[tool]`/`[tool-result]`/`[retry]`) + hint |
| `--json` |             | `{agentId, assistant, exit}` | diagnostics                                                             |
| `--json` | `--verbose` | `{agentId, assistant, exit}` | procedure stream + diagnostics                                          |

- The final assistant reply always goes to **stdout**; it is never streamed to
  stderr. In `--verbose` mode the reasoning/tools stream **live** to stderr and
  the reply is printed to stdout at completion — so users watching stderr
  should wait for the run to finish to see the reply.
- The JSON object **never contains `reasoning`**. `--verbose --json` streams
  the procedure to stderr but leaves the object unchanged.
- Warnings and errors always go to stderr.

`--json` example:

```json
{
  "agentId": "a3f1b2c4-5678-90ab-cdef-1234567890ab",
  "assistant": "The project is a …",
  "exit": "success"
}
```

`exit` is one of `success`, `error`, `max_rounds`, `cancelled`,
`budget_exceeded`, `failover_chain_exhausted`. On a terminal error, `assistant`
reflects whatever was
emitted before the failure (may be partial). If the daemon is unreachable, the
agent id is unknown, or `post_message` fails, no JSON object is emitted — the
error is printed to stderr and the exit code is `1`.

```bash
kallip-run --json --prompt "Summarize the project" | jq -r .assistant
```

## Continuing a session

The target agent persists after the run. Its id is printed in the completion
hint, and you can continue the same session:

```
$ kallip-run --prompt "Summarize the project"
The project is a ...

agent a3f1b2c4-5678-90ab-cdef-1234567890ab finished. Continue with: kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "<prompt>"
$ kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "and its license?"
```

Pass `--verbose` to watch the agent's reasoning and tool calls as it works:

```bash
kallip-run --verbose --prompt "Summarize the project"
```

A follow-up via `--agent` keeps the agent's full context. It works only against
a daemon that still has the agent registered — the same instance, or one that
restored it from disk on startup. `--agent` does not validate the id format; an
unknown id surfaces as a daemon error.

For the complete environment variable reference including LLM provider
configuration, see [env.md](env.md).
