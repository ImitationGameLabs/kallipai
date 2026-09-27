---
title: Platform services
description: Optional rolling file logging for the archeion, lesche, files, instances, and model gateway service processes.
order: 30
---

Opt-in rolling file logs for the platform service processes
(archeion, lesche, files, instances, and the model gateway). Each
variable makes that service's daily-rotating file the only event
channel; stdout carries a single signpost line and goes quiet.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `KALLIPAI_ARCHEION_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the archeion. Set to a directory: the daily-rotating file (seven kept, `archeion.<date>.log`) becomes the only event channel, and stdout carries a single signpost line naming the service, the active file, and the level threshold, then goes quiet. A directory that cannot be created, or an appender that fails to build, exits non-zero with a stderr line. Unset or empty keeps the stdout-only behavior. Directory ownership and permissions are a deployment concern (systemd `LogsDirectory`). |
| `KALLIPAI_LESCHE_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the lesche, identical semantics to `KALLIPAI_ARCHEION_LOG_DIR` (file-only events, daily rotation, seven files kept, `lesche.<date>.log`). |
| `KALLIPAI_FILES_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the files service, identical semantics to `KALLIPAI_ARCHEION_LOG_DIR` (file-only events, daily rotation, seven files kept, `files.<date>.log`). |
| `KALLIPAI_INSTANCES_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the instances proxy, identical semantics to `KALLIPAI_ARCHEION_LOG_DIR` (file-only events, daily rotation, seven files kept, `instances.<date>.log`). |
| `KALLIPAI_MODEL_GATEWAY_LOG_DIR` | no | _(unset; stdout only)_ | Opt-in rolling file log for the model gateway, identical semantics to `KALLIPAI_ARCHEION_LOG_DIR` (file-only events, daily rotation, seven files kept, `model-gateway.<date>.log`). |
