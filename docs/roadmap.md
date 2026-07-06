# Roadmap

kallipai is a daemon-centric agent runtime aimed at cross-project, multi-agent
coordination - early stage, not yet ready for production use. This document
captures where the project is headed; see [architecture.md](architecture.md)
for how it works today.

## P1 - Production-grade local agent team

_Goal: a local agent team that meets basic agent needs, like coding - built on
agentic design._

- **Agentic agent management** - agents spawn and manage other agents; the team
  organizes itself, not driven by a human operator top-down
- **Non-blocking permission control** - risky actions go through async approval
  (deferred), so the agent keeps working while approval happens elsewhere
- **Inter-agent communication** - async messaging between agents
- **Self-managed agentic context** - each agent controls its own context
  (pin / evict), not silently summarized by the system
- **Team-evolving skill system** - skills accumulate from the team's experience
  and evolve with it, shared across the team
- **Misuse-resistant toolset** - tools shaped so agents rarely misuse them
- **Production hardening** - reliability, persistence, observability,
  cost/usage tracking, test coverage

## P2 - Scalable, remotely-managed agent team

_Goal: agent teams that deploy and scale fast, manageable on the go via mobile
and cloud._

- **Deployment & scaling** - quickly spin up and scale out agent teams
- **Mobile app** - manage agent teams from your mobile device
- **Cloud control** - bridge mobile clients to teams across instances

## P3 - Agent company

_Goal: a zero-employee company - run entirely by agents, serving its own
customers and taking on work as one entity._

- **Multi-team composition** - the company is built from multiple coordinating
  teams (the deployable units from P2)
- **Customer-facing** - it has its own customers, takes on tasks and projects,
  and delivers outcomes
- **Fully autonomous** - run end-to-end by agents with no human employees;
  humans are customers, not staff
- **Organizational memory** - roles, hierarchy, and knowledge persist as the
  company's, beyond any single agent

## Technical considerations

- **Sandbox breadth** - the dynamic FS scoping and per-directory write-mutex
  core (a read/write lock so only one agent may hold write access to a
  directory at a time, for multi-agent concurrency safety) has landed,
  enforced per spawned shell via landlock on Linux. Still-open isolation axes:
  cgroup resource limits, network egress control, overlayfs copy-on-write
  staging, and a secret-proxy tool.
- **Tool-call metrics for harness refinement** - tool-call outcomes (command
  exit codes and the misuse they imply, success rates, failure patterns) are a
  feedback signal, not a model ranking: with the model held fixed, they show
  which of our tool and CLI designs lead the agent into misuse, guiding
  continuous refinement toward rarer misuse
