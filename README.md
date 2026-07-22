# Kallip AI

> **Early stage.** Not ready for production use.

An agent harness designed from the start as a multi-agent system. Agents manage their own subagents and context, and coordinate across projects.

## Not another coding assistant

Existing coding assistants excel at single-project, single-session work.
kallip aims at a different problem: **cross-project, multi-agent coordination**
— without being a "multi-agent manager."

Instead of a top-down orchestration layer, kallip is driven through a
headless CLI: the agent itself decides when to spawn, switch between, and
coordinate multiple agent instances across projects.

For planned direction, see the [roadmap](docs/roadmap.md).

## Architecture

| Crate            | Description                                                                           |
| ---------------- | ------------------------------------------------------------------------------------- |
| `kallip-common`  | Shared types and command parsing                                                      |
| `kallip-runtime` | Agent runtime: agent context management, policy engine, tool dispatch                 |
| `kallip-shell`   | Provider-neutral shell/session tools for LLM applications (used by the runtime)       |
| `kallip-tagma`   | HTTP API server hosting multiple agent instances                                      |
| `kallip-client`  | Async client library for the tagma HTTP API                                           |
| `kallip`         | Headless CLI — designed for agents to call, so an agent can manage other agents       |
| `kallip-tui`     | Interactive terminal UI for human users, with approval prompts and markdown rendering |
| `kallip-run`     | Agent runner for scripting and automation — CI, benchmarks, scripted workflows        |

> Note: `just-llm-client` is an unrelated upstream crate, not part of this project.

## Agentic context management

The most experimental part of the design. Context management in kallip is not
hidden behind heuristics — the agent manages its own attention explicitly
through tools:

| Tool             | What it does                                                               |
| ---------------- | -------------------------------------------------------------------------- |
| `context_pin`    | Mark content as essential — pinned items survive eviction                  |
| `context_unpin`  | Remove the pin, allowing the content to be evicted again                   |
| `context_evict`  | Evict all turns, replacing them with a summary pinned as `context_summary` |
| `context_status` | Inspect current token usage and pinned items                               |

To make this concrete: context compaction maps directly to `context_evict` —
the agent writes a summary preserving key facts, and the tool atomically pins
the summary and evicts all turns. Compaction is not a hidden heuristic but an
explicit agent action.

We openly acknowledge that this approach is **unproven** — it may or may not
outperform traditional summarization-only context strategies. But it enables
something interesting: **skills that emerge naturally.**

### Emergent skills

When the agent accumulates experience — say, effective patterns for using a
particular CLI tool or debugging a class of issues — it can distill that into a
file (`~/.local/share/kallip/skills/<name>.md`). Later, when it encounters a
matching situation, it reads the file and pins the content. No dedicated skill
system is needed — file read + pin naturally forms skill management.

## Asynchronous approval

When a tool call is classified as risky (e.g. `rm -rf`, `sudo`, `git push --force`),
the agent does not block waiting for a human. Instead, the call is deferred and
the agent manages the lifecycle through tools:

| Tool              | What it does                                            |
| ----------------- | ------------------------------------------------------- |
| `approval_list`   | List approvals, optionally filtered by status           |
| `approval_commit` | Submit a pending action for approval with justification |
| `approval_redeem` | Execute a previously approved action                    |
| `approval_cancel` | Abandon an approval that is no longer needed            |

The flow:

1. The tool call is **deferred** — stored in a queue with an approval ID.
2. A deferred result is returned to the LLM immediately, so the agent can
   continue working on other things.
3. An `ApprovalUpdated` SSE event is emitted, visible to any client
   (TUI, CLI, or a supervisor agent).
4. The client approves or denies the request via the tagma's approval API
   (`GET /approvals`, `POST /approvals/{id}`).
5. On the next agent round, the approval notification is injected into context.
   The agent then calls `approval_redeem` to execute the stored action.

This design is intentional for multi-agent scenarios: a supervisor agent can monitor
approvals from its subagents and make approval decisions programmatically,
without a human in the loop. Or it can surface the decision to a human.

## Quick start

```bash
KALLIP_LLM_PROVIDER=deepseek \
KALLIP_LLM_MODEL=deepseek-v4-flash \
KALLIP_LLM_DEEPSEEK_API_KEY=your-key \
cargo run -p kallip-tagma

# TUI client
cargo run -p kallip-tui
```
