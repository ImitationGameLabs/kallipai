---
title: Agent shell sessions
description: Variables the tagma injects into every agent shell session.
order: 20
---

## Variables Injected into Agent Shell Sessions

The tagma injects these into each agent's shell environment so that CLI commands
run inside an agent's shell can communicate with the tagma. They are not set by
the operator; the tagma provides them automatically.

| Variable                     | Injection point                     | Description                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KALLIP_TAGMA_URL`           | Tagma process (`main.rs`)           | Copied from `KALLIP_ADVERTISE_URL` at startup via `set_var`. Inherited by child processes. Read by CLI clients to connect.                                                                                                                                                                                                                           |
| `KALLIP_AUTH_TOKEN`          | Per-agent shell (`routes/agent.rs`) | Generated 256-bit `sk-agent-…` authentication token. Injected into shell sessions so the agent can call back to the tagma; the tagma stores and compares only a hash of it. The CLI requires it.                                                                                                                                                       |
| `KALLIP_ID`                  | Per-agent shell (`routes/agent.rs`) | UUID of the current agent. Available inside agent shells. Read by the CLI for the `skill` and `subagent` subcommands (where it identifies the acting supervisor), and as the self-target for `activity` and `lesche send`.                                                                                                                           |
| `KALLIP_SUPERVISOR_AGENT_ID` | Per-agent shell (`routes/agent.rs`) | The agent's supervisor id (the direct `created_by` delegator). Injected for subagents only: **unset for the root agent** (absent, not empty), so root-ness is detectable by env absence. Surfaces the id so the agent can address its supervisor (e.g. `kallip message <id>`); the CLI takes the id as a positional arg and does not read this var. |
| `KALLIP_ROOT_AGENT_ID`       | Per-agent shell (`routes/agent.rs`) | The tagma root agent id (the agent itself for the root). Injected into every agent's shell. Surfaces the id so the agent can escalate to the root (e.g. `kallip message <id>`); the CLI takes the id as a positional arg and does not read this var.                                                                                                 |

`KALLIP_TAGMA_URL` is also self-set by the tagma from `KALLIP_ADVERTISE_URL`
at startup; see [Tagma service](../../configuration/tagma/service.md) for the
difference between the two variables.
