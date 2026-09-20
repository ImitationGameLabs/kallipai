---
title: Scheduled tasks (cron)
description: Configuration for the scheduled-task service (cron).
order: 40
---

A small service that fires schedules on time and injects each firing into
the target agent's conversation, plus the `kallip-cron` command-line client
agents use to manage their own schedules. The client runs inside an agent
shell and reuses the shell's injected credentials (`KALLIP_ID` +
`KALLIP_AUTH_TOKEN`); every operation is scoped to that agent's own
schedules, and the management API listens on loopback only.

| Variable                 | Required  | Default                          | Description                                                                                                                                                              |
| ------------------------ | --------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KALLIP_CRON_ADDR`       | no        | `127.0.0.1:3010`                 | Address the daemon's management API listens on. **Loopback only**: cron is an internal tagma-side service; the daemon refuses a non-loopback bind.                      |
| `KALLIP_CRON_DATA_DIR`   | no        | Platform data dir + `kallipai/cron/` | Directory for the service's schedule database. Unset with no determinable platform data dir, the service fails fast instead of guessing.                                                              |
| `KALLIP_CRON_TICK_MS`    | no        | `1000`                           | Scheduler tick interval (ms). Must be `>= 1000` (second-precision scheduler).                                                                                            |
| `KALLIP_CRON_DELIVER_MS` | no        | `500`                            | Deliverer poll interval (ms): how often triggered schedules are pushed to tagma.                                                                                         |
| `KALLIP_CRON_URL`        | no        | `http://127.0.0.1:3010`          | Daemon URL used by the `kallip-cron` CLI client.                                                                                                                         |
| `KALLIP_ID`              | yes (CLI) | _(unset)_                        | The calling agent's id (auto-injected into agent shells by the tagma); the CLI passes it as the self-scope, and the daemon verifies it against the bearer via the tagma. |
| `KALLIP_TAGMA_URL`       | yes       | `http://127.0.0.1:3000`          | tagma URL for delivery + per-request verify. Reused from the tagma client; not `KALLIP_CRON_*`-prefixed.                               |
| `KALLIP_AUTH_TOKEN`      | yes       | _(unset)_                        | The daemon's operator secret for delivery (fired reminders render `[From: operator]`); the CLI's agent bearer for management requests. Reused from the tagma client.     |
