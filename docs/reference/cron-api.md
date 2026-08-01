# `kallip-cron` HTTP API

The timer/notification daemon `kallip-cron-daemon` hosts a small management HTTP
API (loopback only, default `127.0.0.1:3010`) consumed by the `kallip-cron` CLI.
When a schedule fires, the daemon injects its `message` into the target agent
conversation via the tagma HTTP API (`POST /agents/{id}/message`), not through
this API.

## Auth — agent-token verification (self-scoped)

There is **no cron-specific token**. The daemon is loopback-only (it refuses a
non-loopback bind at startup); the boundary is per-request agent-token
verification:

- Every request carries `Authorization: Bearer <agent-token>` and the caller's
  claimed agent id — `agent_id` in the `POST /schedules` body, `?agent=<id>`
  query on the read/delete ops.
- The daemon forwards the pair to the tagma's
  `GET /agents/{id}/verify` (204 on match, 401 otherwise) and scopes the
  operation to that agent's **own** schedules.
- `401` = the claimed id does not match the token; `503` = the tagma is
  unreachable (not an auth failure); `400` = missing bearer/agent.

The `kallip-cron` CLI is env-driven, like `kallip`: it reads `KALLIP_AUTH_TOKEN`
(bearer) and `KALLIP_ID` (claimed id) from the agent shell (both auto-injected
by the tagma). Schedules are self-managed: an agent can only create, list, or
mutate its own schedules; an operator bearer is rejected (no agent match).

All errors are `{"error":{"message":"..."}}` with the status on the response
line.

## Precision contract

Times are UTC, second-precision. `tick_ms` is `>= 1000`; sub-second `In`
durations and `at_time` with a seconds component are rejected. `at_time`
(`"HH:MM"` UTC) is accepted only for `daily`/`monthly`/`yearly` periods.

## Endpoints

### `GET /health`

Returns `OK` (plain text). Unauthenticated.

### `GET /status?agent=`

Status scoped to `agent`: that agent's active count, pending-triggered count,
next fire time.

```json
{ "healthy": true, "active_schedules": 3, "pending_triggered": 0, "next_fire": "2025-12-25T09:00:00Z" }
```

### `POST /schedules`

Create a schedule owned by (and targeting) `agent_id`. The server mints the id
(UUID v4) and the initial `next_fire`. `201` returns the schedule.

```json
{
  "name": "standup reminder",
  "trigger": { "type": "every", "period": "daily", "at_time": "09:00" },
  "agent_id": "<your-agent-id>",
  "message": "Time for standup.",
  "tags": ["work"],
  "priority": "normal"
}
```

Trigger shapes:

- `{ "type": "once", "at": "<RFC3339>" }` — one-shot at an absolute time (a past
  time fires on the next tick: fire-ASAP for a missed reminder).
- `{ "type": "in", "duration_seconds": 300 }` — one-shot N seconds from create.
- `{ "type": "every", "period": "minutely|hourly|daily|weekly|monthly|yearly", "at_time": "09:00" }`
  — recurring. `at_time` only for daily/monthly/yearly.

### `GET /schedules?agent=&status=&tag=`

List `agent`'s schedules, optionally filtered by status (`active`/`paused`/
`completed`/`triggered`) and/or tag.

```json
{ "schedules": [ { "id": "...", ... } ], "total": 1 }
```

### `GET /schedules/next?agent=`

`agent`'s earliest-fire active schedule, or `null`.

### `GET /schedules/{id}?agent=`

One of `agent`'s schedules, or `404`. Cross-owner is indistinguishable from
not-found (uniform `404` — no ownership oracle).

### `PATCH /schedules/{id}?agent=`

Status-only update (pause/resume). `next_fire`/`last_fire` are never
client-mutable — this is what prevents a fired one-timer from being re-armed.
Cross-owner → `404`.

```json
{ "status": "paused" }
```

### `DELETE /schedules/{id}?agent=`

`204` on success, `404` if not found or owned by another agent.

## Delivery semantics

At-least-once. tagma's `post_message` does not dedup, so a daemon crash in the
post → ack window can double-deliver one row; per-id ack + persisted 503-backoff
narrow the window. On a 503 (agent prompt queue full), the row backs off
exponentially (1s, 2s, 4s, … capped at 60s, persisted across restart) and the
deliverer moves on. Fired reminders are delivered with the daemon's operator
token and render `[From: operator]` in the conversation.
