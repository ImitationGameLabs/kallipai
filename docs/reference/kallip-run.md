# `kallip-run` Reference

Agent runner for scripting and automation. Creates an agent (or resumes an
existing one with `--agent`), prints the final assistant reply to stdout, and
exits with a semantic exit code. Designed for scripted and automated workflows
where the caller needs machine-readable output and exit-status-driven control
flow.

By default the agent is **preserved** after completion so that logs, history,
and token usage remain available for auditing, and so it can be resumed with
`--agent`. Pass `--remove` to archive the agent (history and usage preserved)
after the run finishes.

```bash
kallip-run [OPTIONS] --prompt <PROMPT>
```

Uses `KALLIP_AUTH_TOKEN` (mandatory) and `KALLIP_DAEMON_URL`
(env, default `http://127.0.0.1:3000`).

## Options

| Flag                     | Description                                                      |
| ------------------------ | ---------------------------------------------------------------- |
| `--prompt <PROMPT>`      | The prompt to send to the agent (required)                       |
| `--workspace-root <DIR>` | Working directory for the agent (spawn only)                     |
| `--max-rounds <N>`       | Maximum tool-call rounds (spawn only; overrides daemon default)  |
| `--agent <ID>`           | Resume an existing agent by id instead of spawning               |
| `--json`                 | Emit a single JSON object on stdout (see Output)                 |
| `--verbose`              | Stream the agent's procedure (reasoning, tool calls) to stderr   |
| `--remove`               | Archive the agent (history and usage preserved) after completion |

`--workspace-root` and `--max-rounds` apply only when spawning a new agent and
are ignored when `--agent` is given.

## Exit codes

| Code | Meaning               |
| ---- | --------------------- |
| 0    | Success               |
| 1    | Error                 |
| 2    | Max rounds exceeded   |
| 3    | Cancelled             |
| 4    | Token budget exceeded |

## Output

The output shape is driven by `--json` and `--verbose` (there is no TTY-based
auto-detection). The daemon already persists the agent's full execution
history, so by default the runner is **minimal**: just the final reply.

| `--json` | `--verbose` | stdout                                | stderr                                                                  |
| -------- | ----------- | ------------------------------------- | ----------------------------------------------------------------------- |
|          |             | final assistant reply                 | completion hint (agent id + how to continue)                            |
|          | `--verbose` | final assistant reply                 | the procedure (`[reasoning]`/`[tool]`/`[tool-result]`/`[retry]`) + hint |
| `--json` |             | `{agentId, assistant, exit, removed}` | diagnostics                                                             |
| `--json` | `--verbose` | `{agentId, assistant, exit, removed}` | procedure stream + diagnostics                                          |

- The final assistant reply always goes to **stdout**; it is never streamed to
  stderr. In `--verbose` mode the reasoning/tools stream **live** to stderr and
  the reply is printed to stdout at completion — so users watching stderr
  should wait for the run to finish to see the reply.
- The JSON object **never contains `reasoning`**. `--verbose --json` streams
  the procedure to stderr but leaves the object unchanged.
- Warnings and errors always go to stderr, so a failed `--remove` is not silent
  (also reflected in the `removed` field).

`--json` example:

```json
{
  "agentId": "a3f1b2c4-5678-90ab-cdef-1234567890ab",
  "assistant": "The project is a …",
  "exit": "success",
  "removed": false
}
```

`exit` is one of `success`, `error`, `max_rounds`, `cancelled`,
`budget_exceeded`. `removed` is `true` only when `--remove` archived the agent
after the run. On a terminal error, `assistant` reflects whatever was emitted
before the failure (may be partial). If the daemon is unreachable, the agent id
is unknown, or `post_message` fails, no JSON object is emitted — the error is
printed to stderr and the exit code is `1`.

```bash
kallip-run --json --prompt "Summarize the project" | jq -r .assistant
```

## Resuming an agent

By default the agent is **preserved** after the run. Its id is printed in the
completion hint, and you can continue the same session:

```
$ kallip-run --prompt "Summarize the project"
The project is a ...

agent a3f1b2c4-5678-90ab-cdef-1234567890ab finished (kept). Continue with: kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "<prompt>"
$ kallip-run --agent a3f1b2c4-5678-90ab-cdef-1234567890ab --prompt "and its license?"
```

Pass `--verbose` to watch the agent's reasoning and tool calls as it works:

```bash
kallip-run --verbose --prompt "Summarize the project"
```

Resume sends the prompt as a follow-up message to the existing agent, which
keeps its full context. It works only against a daemon that still has the agent
registered — the same instance that created it, or one that restored it from
disk on startup. Resuming against a different daemon (or one that has not
restored the agent) returns an error. `--agent` does not validate the id
format; an unknown id surfaces as a daemon error.

Use `--remove` to archive the agent immediately after the run:

```bash
kallip-run --remove --prompt "Summarize the project"
```

For the complete environment variable reference including LLM provider
configuration, see [env.md](env.md).
