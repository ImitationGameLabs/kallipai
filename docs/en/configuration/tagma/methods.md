---
title: Model configuration methods
description: Configure models through the web interface, the `kallip profile-set` command, or manual `profiles.toml` editing.
order: 21
---

Part of the [Configuration reference](../index.md); this page covers the three
ways to configure models. For the three-layer model (what a provider, profile,
and profile set are, and why each is its own layer), see
[Model configuration](llm.md).

## Web Interface

The Profiles page in the manage area is the day-to-day configuration surface.
It maintains the endpoint pool (protocol family, key, `base_url`), organizes
profile cards between named sets and a parking area by drag and drop, declares
each profile's context window and modalities, transfers the default-set
marker, and can run a connectivity probe against a set or a single profile.

Saving writes the instance's `profiles.toml` and takes effect immediately.
Deleting a set that still has bound agents requires a confirmation. The full
field reference lives in the manual-editing section below.

## The `kallip profile-set` Command

Four subcommands cover inspection, rebinding, marker transfer, and removal:

- `kallip profile-set list`: list the configured profile sets, their profile
  counts, and the default marker.
- `kallip profile-set bind <ID> <SET>`: rebind an agent to a named set. A live
  agent swaps its failover chain on its next wake-up; a parked one resolves it
  at restore.
- `kallip profile-set default <SET>`: transfer the default-set marker to an
  existing set.
- `kallip profile-set remove <SET>`: remove a set. The default set and the
  root's set are refused; other referenced sets list their bound agents and
  need `--force` (they are interrupted, then keep a dangling record until
  rebound).

`bind` requires the exact set name; an unknown name lists the available sets.

## Manual `profiles.toml` Editing

### File Location

The profiles config file lives at the instance config root's
`profiles.toml`: `<config home>/kallipai/tagmata/<slug>/`.
A config root that cannot be derived (no `KALLIP_TAGMA_SLUG`, no config
home) degrades the read to the implicit env profile with a warning;
a write with no config root errors out. Without a config file (the
default for benchmark/scripting via Harbor, an external agent
benchmarking framework, and `kallip-run`), a single implicit profile is
used: its `max_context_window` is derived from `KALLIP_CONTEXT_WINDOW_TOKENS`
(default `128000`).

With a config file, the tagma loads multiple provider/model combinations, each
profile declaring its own `max_context_window`.
Each profile may also declare `modalities`; an omitted declaration
defaults to text-only, and a set's effective modalities are the
intersection across its members.

### Example

Example `profiles.toml`:

```toml
default = "primary"

[endpoints.deepseek-primary]
family = "deepseek"
api_key = "${KALLIP_LLM_DEEPSEEK_API_KEY}" # env-var indirection keeps secrets out of the file

[endpoints.openrouter]
family = "openai-compatible"
api_key = "${OPENROUTER_API_KEY}"
base_url = "https://openrouter.ai/api/v1"

[endpoints.official-openai]
family = "openai-responses"
api_key = "${OPENAI_API_KEY}" # no base_url: talks to the official endpoint

[sets.primary]
description = "full-capability work"
  [[sets.primary.profiles]]
  id = "deepseek-v4-pro"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-pro"
  max_context_window = 500000

[sets.fast]
description = "cheap delegation"
  [[sets.fast.profiles]]
  id = "deepseek-v4-flash"
  endpoint = "deepseek-primary"
  model = "deepseek-v4-flash"
  max_context_window = 128000
```

### Fields

- `family` must be one of `deepseek`, `openai-compatible`, `openai-responses`, `anthropic`.
- `base_url` is required for `openai-compatible` and optional elsewhere: every other family falls back to its official endpoint when omitted.
- `store` (optional, default `true`) keeps server-side conversation storage on, so `openai-responses` continues each turn from the stored chain; `store = false` (or any non-Responses family) replays the full conversation every turn.
- `effort` (optional) requests a reasoning effort: `low`, `medium`, `high`, `xhigh`, or `max`. Providers honor it within their own limits (DeepSeek officially downgrades `medium` and `xhigh` to `high` per its [API reference](https://api-docs.deepseek.com/api/create-chat-completion)).
- `${VAR}` in `api_key` / `base_url` is expanded from the process environment.
- The config file should be `chmod 600` (the tagma warns if
  group/other-readable, since it may hold API keys).

### Set Selection

Sets are addressed by name: the root agent is bound to the config's `default`
set, and each subagent spawn declares its set explicitly via `profile_set`
(an unknown name is rejected). The binding is recorded on the agent record,
so set order in the file carries no meaning. Renaming or deleting a set
leaves bound agents dangling; restore tolerates the placeholder, but
prompt delivery rejects with `409` until the set returns under that name.

The `default` marker resolves at load: an explicit `default = "<name>"` must
name an existing set (a name matching nothing is a config error); with exactly
one set and no marker, that set becomes the default and the tagma writes the
marker back into the file, inserted ahead of the first table header, so
comments, key order, and `${VAR}` spellings survive; several sets with no
marker is a config error (name one explicitly). A hand-written empty
marker (`default = ""`) reads back as unmarked and takes the single-set
auto-mark path rather than erroring. In that corner the file keeps its
explicit empty marker; the auto-mark write-back is skipped, so the
resolved default is memory-only. An empty `sets` table boots
profile-less.

The selected set's first profile is the active model; the remaining profiles
form a within-set failover chain. When the active profile fails terminally
(HTTP 401/403/404, or transient retries exhausted), the runner advances to the
next profile in the set and retries the same turn; a request-level failure
(400/422) errors the round instead. The active profile index sticks for the
agent's lifetime and resets to 0 on restore.

On advance, the context window tracks the new profile's declared
`max_context_window` (within-set windows may differ; placing models with
different windows in one set is supported). If the carried context now exceeds
the new (possibly smaller) window, the runner compacts it before retrying, so
the turn survives the switch. A candidate whose window would violate a budget
invariant is skipped _before_ the advance (so the agent never sends an oversized
request to a smaller-window model); if no feasible candidate remains, the chain
is reported as an infeasible-window error: every candidate conflicts with
the budget (tune `KALLIP_SUMMARY_MAX_TOKENS` / `KALLIP_PINNED_BUDGET_RATIO` or raise the window). The _active_ profile's window (not a
failover candidate) is validated at spawn: a window that violates a budget
invariant rejects the spawn outright (fail-fast) rather than silently falling
back.

The retry budget is **per-endpoint**, not per-profile: rate limits are
endpoint-scoped, so two profiles sharing one endpoint share one budget. A
profile's transient retries accumulate within `retry_timeout` (the retry
deadline that `KALLIP_RETRY_TIMEOUT_SECS`, default 300, configures)
**across rounds**. This is intentional rate-limit backpressure (a persistently failing endpoint
gets fewer retries, forcing failover or a round error), and it matches the
pre-failover agent-wide behavior for the active profile. The index only advances
forward, so a failed-over-from endpoint's accumulated budget never re-bites.

Edge case: an agent whose recorded set name matches no live set dangles: the
record keeps the name, restore tolerates it as a placeholder, but prompt
delivery rejects with `409` until the set returns under that name.
