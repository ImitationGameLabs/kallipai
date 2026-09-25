---
title: Files service
description: Configuration for the files service.
order: 50
---

The files service stores content: a content-addressed blob store with
record metadata in its own database, and the `kallip file` command-line
client for everyday use. The client reads its credentials from the agent
shell's environment; no flags carry secrets. Credentials between the
platform services are provisioned automatically by the deployment; there
is nothing to configure by hand.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `KALLIPAI_FILES_ADDR` | no | `127.0.0.1:7400` | Address the service listens on (behind a TLS-terminating reverse proxy). |
| `KALLIPAI_FILES_BLOB_ROOT` | yes (service) | _(unset)_ | Root directory of the content-addressed blob store; created on demand. |
| `KALLIPAI_FILES_DATABASE_URL` | yes (service) | _(unset)_ | Postgres URL for the metadata store; a missing URL fails fast at boot. |
| `KALLIPAI_FILES_MAX_BODY_SIZE_MB` | no | `100` | Maximum accepted upload body, in megabytes; larger streams are cut off with 413. |
| `KALLIPAI_FILES_DEGRADE` | no | `closed` | Archeion degrade posture: `closed` fails authorization with 503 when the registry cannot answer; `soft` degrades to deny (403). Neither posture weakens credential verification. |
| `KALLIPAI_FILES_GC_INTERVAL_SECS` | no | `60` | Delay between GC passes (sweep + reconcile), in seconds. |
| `KALLIPAI_FILES_GC_GRACE_SECS` | no | `60` | How long a zero-refcount row must have been freed before the GC may reclaim it. |
| `KALLIPAI_FILES_GC_BATCH` | no | `128` | Maximum catalog rows reclaimed per GC pass. |
