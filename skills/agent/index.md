---
name: Agent Skills Index
description: Navigation index for agent self-management skills
---
# Agent Skills

Skills for managing your own runtime — the agent system CLI, context window, and the skill system itself.

| Skill | When to use |
|-------|-------------|
| `kallip` | Any coordination with the daemon: status, subagents, messages, approvals, policy, budget, skills |
| `context-management` | Context window getting heavy; deciding what to pin/unpin/evict |
| `skill-management` | Finding, creating, or organizing skills; understanding the index tree |
| `subagent-management` | Spawning, messaging, monitoring, or cleaning up subagents; permission classes and dirlock |

## How to choose

- Need to talk to the daemon or manage subagents? → `kallip`
- Context budget running low or unsure what to keep? → `context-management`
- Looking for a skill or want to create one? → `skill-management`
- Spawning subagents or testing sandbox permissions? → `subagent-management`

This index is a navigation aid — read it, don't pin it.
