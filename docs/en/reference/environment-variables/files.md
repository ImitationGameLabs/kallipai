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
| `KALLIP_FILES_ADDR` | no | `127.0.0.1:7400` | Address the service listens on (behind a TLS-terminating reverse proxy). |
| `KALLIP_FILES_BLOB_ROOT` | yes (service) | _(unset)_ | Root directory of the content-addressed blob store; created on demand. |
| `KALLIP_FILES_DATABASE_URL` | yes (service) | _(unset)_ | Postgres URL for the metadata store; a missing URL fails fast at boot. |
| `KALLIP_FILES_MAX_BODY_SIZE_MB` | no | `100` | Maximum accepted upload body, in megabytes; larger streams are cut off with 413. |
| `KALLIP_FILES_DEGRADE` | no | `closed` | Archeion degrade posture: `closed` fails authorization with 503 when the registry cannot answer; `soft` degrades to deny (403). Neither posture weakens credential verification. |
| `KALLIP_FILES_GC_INTERVAL_SECS` | no | `60` | Delay between GC passes (sweep + reconcile), in seconds. |
| `KALLIP_FILES_GC_GRACE_SECS` | no | `60` | How long a zero-refcount row must have been freed before the GC may reclaim it. |
| `KALLIP_FILES_GC_BATCH` | no | `128` | Maximum catalog rows reclaimed per GC pass. |
