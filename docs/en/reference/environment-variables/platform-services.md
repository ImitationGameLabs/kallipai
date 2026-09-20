---
title: Platform services
description: Optional rolling file logging for the archeion, lesche, and files service processes.
order: 30
---

Opt-in rolling file logs for the three platform service processes
(archeion, lesche, files). Each variable redirects that service's log output
into daily-rotating files while stdout keeps flowing.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `KALLIP_ARCHEION_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the archeion. Set to a directory: events are double-written to daily-rotating files (seven kept, `archeion.log.` prefix) while stdout keeps flowing for journald capture. Unset or empty keeps the historical stdout-only behavior; an uncreatable directory degrades to stdout-only with a startup notice. Directory ownership and permissions are a deployment concern (systemd `LogsDirectory`). |
| `KALLIP_LESCHE_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the lesche, identical semantics to `KALLIP_ARCHEION_LOG_DIR` (double-write, daily rotation, seven files kept, `lesche.log.` prefix). |
| `KALLIP_FILES_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the files service, identical semantics to `KALLIP_ARCHEION_LOG_DIR` (double-write, daily rotation, seven files kept, `files.log.` prefix). |
