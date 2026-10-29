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
It maintains the provider pool (`[endpoints.<id>]` in the file: protocol
family, key, `base_url`, and the optional `model_list_base_url`), organizes
profile cards between named sets and the Unassigned section by drag and drop,
declares each profile's context window and modalities, transfers the default-set
marker, and can run a connectivity probe against a set or a single profile.
Saving applies immediately and writes the instance's `profiles.toml` on
a best-effort basis (a failed write is logged).
When the profile source is the model gateway, the page turns into a read-only
mirror of the gateway's config; the tabs at the top switch between the two
sources (one confirmation each way; see below). Deleting a set that still
has bound agents requires a confirmation. The full field reference lives in
the manual-editing section below.

## The `kallip profile-set` Command

Two subcommands cover inspection and rebinding:

- `kallip profile-set list`: list the configured profile sets, their profile
  counts, and the default marker.
- `kallip profile-set bind <ID> <SET>`: rebind an agent to a named set. A live
  agent swaps its failover chain on its next wake-up; a parked one resolves it
  at restore.

`bind` requires the exact set name; an unknown name lists the available sets.

## Manual `profiles.toml` Editing

### File Location

The profiles config file lives at the instance config root's
`profiles.toml`: `<config home>/kallipai/tagmata/<slug>/`.
A config root that cannot be derived (no `KALLIPAI_TAGMA_SLUG`, no config
home) leaves the tagma with no profiles: the read warns and boots
profile-less, and a write with no config root errors out. No config file
means the same profile-less boot: the tagma serves with no model
configured until the Profiles page (or a hand-written file) adds the first
profile.

With a config file, the tagma loads multiple provider/model combinations, each
profile declaring its own `max_context_window`.
Each profile may also declare `modalities`; an omitted declaration
defaults to text-only, and a set's effective modalities are the
intersection across its members.

### Profile Source

Where the config comes from is a separate setting: `settings.toml`, next to
`profiles.toml` in the same directory.

```toml
[profiles.source]
mode = "local"  # or "model-gateway"
```

`local` (the default) reads `profiles.toml` as the single source of truth.
`model-gateway` pulls the config from the model gateway behind the platform
edge: the base derives from the relay entry's origin as
`{origin}/v1/model-gateway` (a multi-entry polis.toml resolves the pinned
entry, or the first enrolled one before the first pin), and the token comes
from the relay enrollment (stored with the entry's credentials). What
arrives is this tagma's selected collection, in one read; a tagma
without a selection pulls an empty config. The pulled config lives in
memory only, `profiles.toml` stops receiving updates, and a cold start
with the gateway unreachable fails with a pointer back to this
setting. The Profiles page's Gateway/Local tabs switch this setting at
runtime; the gateway side is offered only when a relay entry carries an
enrollment token.

An upgraded gateway gives every gateway account space with profiles a home
collection (the collection the row lives in): a space that carried profiles
but no collection gets one named `default`, and a profile that belonged to
no set lands in the space's lexicographically smallest collection. Both
placements are fixed and repeatable, and neither is a decision anyone made:
`default` is a constant in the migration, and a setless profile lands where
the name order puts it. To put a profile in another collection, move it with
`POST /admin/collections/{target}/profiles/{profile_id}/move`, whose body
carries `from_collection_name` (where the row is now) and an optional `set`
(a set in the target collection; giving one makes the arriving row a
member). No web interface moves a row between collections.

Example `profiles.toml`:

```toml
default = "primary"

[endpoints.deepseek-primary]
family = "deepseek"
api_key = "sk-your-deepseek-key" # a literal value

[endpoints.openrouter]
family = "openai-compatible"
api_key = "sk-or-your-openrouter-key"
base_url = "https://openrouter.ai/api/v1"

[endpoints.official-openai]
family = "openai-responses"
api_key = "sk-your-openai-key" # no base_url: talks to the official endpoint

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
- `model_list_base_url` (optional) is the base the provider Test fetches the model list from; it defaults to `base_url` and exists for providers whose list lives at a different base. It takes a base, not the full model-list URL: the client appends `/models`.
- `store` (optional, default `true`) keeps server-side conversation storage on, so `openai-responses` continues each turn from the stored chain; `store = false` (or any non-Responses family) replays the full conversation every turn.
- `effort` (optional) requests a reasoning effort: `low`, `medium`, `high`, `xhigh`, or `max`. Providers honor it within their own limits (DeepSeek officially downgrades `medium` and `xhigh` to `high` per its [API reference](https://api-docs.deepseek.com/api/create-chat-completion)).
- Values are literal: an API key is written into the file itself.
- `[[parking]]` (absent when empty) holds the profiles that belong to no
  set; the web interface lists them under Unassigned, and under the local
  source giving such a profile a set moves it out of this list. Under
  `model-gateway` the list is whatever the gateway reports as parked, so a
  row the gateway put in a set stays listed until its flag clears: saving
  that set's members on the gateway's management face clears it, and so does
  setting the row unparked there.
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
comments, key order, and value spellings survive; several sets with no
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
the budget (tune `KALLIPAI_SUMMARY_MAX_TOKENS` / `KALLIPAI_PINNED_BUDGET_RATIO` or raise the window). The _active_ profile's window (not a
failover candidate) is validated at spawn: a window that violates a budget
invariant rejects the spawn outright (fail-fast) rather than silently falling
back.

The retry budget belongs to the **episode**, not to an endpoint or a profile: an
episode is one stretch from a first attempt to a verdict, and it allows
`max_retries + 1` sends (`KALLIPAI_MAX_RETRIES`, default 10). `retry_timeout`
(`KALLIPAI_RETRY_TIMEOUT_SECS`, default 300) bounds how long that episode stays
on trial, measured from its first attempt; a mid-stream failure continues the
same episode, so the send count and the deadline both cover the whole stretch.
Advancing the chain opens a fresh episode, so the profile that takes over starts
with its own budget and its own deadline. The index only advances forward, so the
chain never returns to the profile it left behind. The upstream fact table is
keyed by profile rather than by endpoint: a verdict recorded against a profile
is readable by every agent running that profile.

**A profile that met a quota or payment wall is paced, not parked forever.**
The verdict is recorded as a fact against the profile, carrying the reset the
upstream promised, or a window measured from the record itself when it
promised none. When no profile in the chain can serve, each sitting inside its
own window, the round parks and arms a wake at the earliest window end on the
chain, so the round comes back on its own. The wake starts a fresh episode,
which is why it claims the window that just opened rather than the window an
earlier round already took. It is a system-origin round: no prompt is added
for it, and nothing is forced.

An external trigger is the faster path and is forced: the round may send
inside a window, and the record restarts from that moment, so the wait after
it is measured again from the latest ask. A send that succeeds clears the
record whoever wrote it, because the clearing acts on the record and never on
its author. While a window runs, the record appears as a single fact in the
status and fleet snapshots: the kind, the upstream's words, the profile it
names, and the reset the upstream gave — shown unless it gave none or named a
moment past the trust bound. The record is written at the moment the chain
advances, keyed by the profile that sent the failing request. Claiming a
window that has already run out writes the record again — the same kind and
the same words, its start moved to that claim and the upstream's reset hint
dropped, since the moment it named has passed — so the clock a reader sees
belongs to the wait in progress.

**A throttle that names no reset moment is not a wall.** The upstream said
later, not that the profile has nothing left to spend, so the same profile
gets one more send — jittered like any retry — before the chain is charged.
Only when that send comes back the same way does the round move on to the next
profile, where the verdict lands as a fact like any other. A 429 that does
name its reset moment is a quota wall and paces its profile straight away.

Current tuning values, not a contract: a record with no upstream reset paces
its profile for 60 seconds, a payment verdict for 300, an upstream reset is
honored up to 86400 seconds past the record, and up to one second of jitter is
added both to retry delays and to the wake.

Edge case: an agent whose recorded set name matches no live set dangles: the
record keeps the name, restore tolerates it as a placeholder, but prompt
delivery rejects with `409` until the set returns under that name.
