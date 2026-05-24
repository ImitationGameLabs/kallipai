# just-agent

> **Early stage.** Not ready for production use.

An agentic AI agent runtime built in Rust — not another coding assistant.

## Why not another Claude Code / Codex / OpenCode?

Those tools excel at single-project, single-session coding assistance. just-agent
aims at a different problem: **cross-project, multi-agent coordination** — and it
does so without being a "multi-agent manager."

Instead of a top-down orchestration layer, just-agent is designed to be driven
**agenticly through a headless CLI**: the agent itself decides when to spawn,
switch between, and coordinate multiple agent instances across projects.

## Architecture

| Crate               | Description                                                                                  |
| ------------------- | -------------------------------------------------------------------------------------------- |
| `just-agent-core`   | Agent runtime: chat completion, context management, policy engine, core tools, tool dispatch |
| `just-agent-daemon` | HTTP API server hosting multiple agent instances                                             |
| `just-agent-client` | Async client library for the daemon HTTP API                                                 |
| `just-agent`        | Headless CLI — designed for agents to call, so an agent can manage other agents              |
| `just-agent-tui`    | Interactive terminal UI for human users, with approval prompts and markdown rendering        |

## Agentic context management

The most experimental part of the design. Context management in just-agent is not
hidden behind heuristics — the agent manages its own attention explicitly
through tools:

| Tool             | What it does                                              |
| ---------------- | --------------------------------------------------------- |
| `context_pin`    | Mark content as essential — pinned items survive eviction |
| `context_unpin`  | Remove the pin, allowing the content to be evicted again  |
| `context_evict`  | Discard older turns to free token budget (respects pins)  |
| `context_status` | Inspect current token usage and pinned items              |

To make this concrete: the `/compact` command common in coding agents is just a
special case of this model — the agent produces a summary, pins it, then evicts
the original turns. The pinned summary survives. But eviction is more general
than compaction: the agent might evict to focus on a different task, not just to
shrink context.

We openly acknowledge that this approach is **unproven** — it may or may not
outperform traditional summarization-only context strategies. But it enables
something interesting: **skills that emerge naturally.**

### Emergent skills

When the agent accumulates experience — say, effective patterns for using a
particular CLI tool or debugging a class of issues — it can distill that into a
file (`.just-agent/skills/<name>/SKILL.md`). Later, when it encounters a
matching situation, it reads the file and pins the content. No dedicated skill
system is needed — file read + pin naturally forms skill management.

## Asynchronous approval

When a tool call is classified as risky (e.g. `rm -rf`, `sudo`, `git push --force`),
the agent does not block waiting for a human. Instead, the call is deferred and
the agent manages the lifecycle through tools:

| Tool              | What it does                                         |
| ----------------- | ---------------------------------------------------- |
| `approval_list`   | List deferred actions, optionally filtered by status |
| `approval_redeem` | Execute a previously approved action                 |
| `approval_cancel` | Abandon a pending action that is no longer needed    |

The flow:

1. The tool call is **deferred** — stored in a queue with a request ID.
2. A deferred result is returned to the LLM immediately, so the agent can
   continue working on other things.
3. The approval request is emitted as an SSE event, visible to any client
   (TUI, CLI, or a parent agent).
4. The client approves or denies the request via the daemon API.
5. On the next agent round, the approval notification is injected into context.
   The agent then calls `approval_redeem` to execute the stored action.

This design is intentional for multi-agent scenarios: a parent agent can monitor
deferred actions from its sub-agents and make approval decisions programmatically,
without a human in the loop. Or it can surface the decision to a human.

## Quick start

```bash
cargo build --workspace

JUST_LLM_PROVIDER=deepseek \
JUST_LLM_MODEL=deepseek-v4-flash \
JUST_LLM_DEEPSEEK_API_KEY=your-key \
cargo run -p just-agent-daemon

# Headless CLI
cargo run -p just-agent -- start --prompt "Show the current working directory."

# Or the TUI
cargo run -p just-agent-tui
```