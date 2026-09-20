---
title: Agent core and shell
description: Agent runtime tuning and the environment injected into agent shell sessions.
order: 30
---

Part of the [Configuration reference](../index.md); this page covers agent runtime tuning and the environment injected
into shell sessions.

## Agent Core

Runtime tuning variables live in the environment variables reference:
[Tagma](../../reference/environment-variables/tagma.md).

### Inter-Variable Constraints

Some variables have cross-validation rules enforced at startup for the
implicit-profile window:

- `KALLIP_OUTPUT_RESERVE_TOKENS` must be strictly less than the active context window.
- `KALLIP_SUMMARY_MAX_TOKENS` must not exceed the pinned budget, calculated as
  `(the context window − KALLIP_OUTPUT_RESERVE_TOKENS) × KALLIP_PINNED_BUDGET_RATIO`.

These are checked at startup against the implicit-profile window
(`KALLIP_CONTEXT_WINDOW_TOKENS`); a config-file profile's window is checked per-profile
at spawn (and again, lazily, on within-set failover). Config-file profile
windows were never validated at tagma startup.

- `KALLIP_CONTEXT_THRESHOLDS` must have at least 2 values, sorted ascending, each in
  `1`–`99`.
- `KALLIP_TOKEN_BUDGET_WARNINGS` must have at least 1 value, sorted ascending, each in
  `1`–`99`.

The injected-shell variable set has its own page:
[Agent shell sessions](../../reference/environment-variables/agent-shell.md).

### System Environment Variables

The shell backend reads these from the process environment and passes them into
every spawned `bash`:

| Variable | Fallback     | Purpose              |
| -------- | ------------ | -------------------- |
| `HOME`   | _(required)_ | User home directory. |
| `PATH`   | _(required)_ | System PATH.         |

The backend also hardcodes `TERM=dumb`, `NO_COLOR=1`, `LS_COLORS=""`,
`CLICOLOR="0"` into every spawned `bash` to suppress color output.
