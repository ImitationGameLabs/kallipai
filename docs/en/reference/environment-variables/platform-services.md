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
| `KALLIPAI_MODEL_GATEWAY_ARCHEION_URL` | no | _(unset; management face closed)_ | The archeion internal root the admin face authenticates against (e.g. `http://127.0.0.1:7100`). Required together with `KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE`; without the pair the management face refuses every request while the distribution and forwarding faces run unaffected. |
| `KALLIPAI_MODEL_GATEWAY_INTERNAL_TOKEN_FILE` | no | _(unset; management face closed)_ | File holding the archeion-internal shared secret, read once at boot; the NixOS module pins the archeion-minted file (`/var/lib/kallipai/archeion/internal-token`), the same one the lesche, files, and instances read. |
| `KALLIPAI_MODEL_GATEWAY_PUBLIC_BASE_URL` | no | `http://127.0.0.1:7501` | The base the gateway publishes as every distributed profile's `base_url`; consumers append their wire path to it (for example `chat/completions`). The `/v1` suffix is a client-side convention: the gateway's own forwarding routes carry no prefix, and the edge host in front of it strips `/v1` before proxying. A base that reaches the data plane directly, with no stripping edge in front, must be the bare origin: a `/v1` in its path answers `404` there. Use the model-gateway host form, never the api-edge `/v1/model-gateway` segment (that segment routes to the management face, which has no forwarding route). |
