---
title: kallip-files HTTP API
description: HTTP endpoints exposed by the kallip-files service.
order: 50
internal: true
---

## kallip-files HTTP API

The file transfer service `kallip-files` hosts a small content API (default
`127.0.0.1:7400`; in deployments it sits behind a TLS-terminating reverse
proxy). Blobs are content-addressed (SHA-256) and deduplicate by
construction; record metadata (paths, owners, reference counts, the
delivery log) lives in the service's own Postgres. Identity and enrollment
facts stay in the archeion, reached through its `/internal/*` ControlPlane API
over a shared secret on the private network — never through a public edge.

### Auth — archeion-verified principals

There is **no files-specific token**. Every content request carries
`Authorization: Bearer <token>`; the service resolves it through the archeion's
`/internal/verify-bearer` (shared-secret guarded) and acts as the resolved
principal:

- `sk-tagma-…` (an enrolled tagma) — the `kallip file` CLI's principal.
- `sk-admin-…` (operator) — management surface only
  (`GET /v1/files/admin/delivery-events`); the admin principal is refused on
  every content operation.
- User session tokens verify on the web face, not the bearer path — a User
  bearer resolves nowhere here.

Authorization is the per-path ACL matrix ([`acl.rs`](https://github.com/ImitationGameLabs/kallipai/blob/main/crates/platform/kallip-files/src/acl.rs)):
every path lives inside a user's space — `/users/{user}/shared` (the
space's shared region), `/users/{user}/tagmas/{tagma}` (one tagma's private
region, `inbox/` inside it), `/users/{user}/inbox` (the user's delivery
landing directory). A tagma is full owner of its own region, read/write in
its owner's `shared/`, and delivery-only everywhere else; user spaces are
mutually isolated (one user can never address another's paths). A listing
or record read can never serve a row the single-record routes would not.

All errors are `{"error":{"message":"..."}}` with the status on the
response line.

### Endpoints

#### `GET /health`

Returns `ok` (plain text). Unauthenticated on purpose — compose
healthcheck, Caddy probe, and acceptance tooling need a liveness answer
without credentials.

#### `PUT /v1/files?path=/users/{user}/...`

Upload content to a space path. The body streams straight into the content
store in one pass (SHA-256 hashed en route), never buffered whole; a body
over `KALLIP_FILES_MAX_BODY_SIZE_MB` is cut off with `413`. Re-uploading
identical bytes deduplicates onto the same blob. `201` returns:

```json
{ "record_id": "<uuid>", "blob_id": "<sha-256 content address>" }
```

#### `GET /v1/files?space=self|shared&prefix=&limit=`

List the caller's records. `space` is `self` (the principal's own region,
inbox included) or `shared` (the space's shared region); `prefix` narrows
to paths under a relative prefix (LIKE metacharacters are escaped — the
match is literal); `limit` can only lower the server's page cap (500).
Returns a JSON array of entries:

```json
[{ "id": "<uuid>", "path": "/users/u1/shared/report.pdf", "size": 1234, "created_at": "2026-08-31T09:00:00Z" }]
```

#### `GET /v1/files/{id}`

Download a record's content. Honors a single-range `Range` header (`206`
with `content-range`; multi-range and unsatisfiable forms answer per RFC —
`416` carries `content-range: bytes */<size>`). Full responses are `200`
with `accept-ranges: bytes`. Unknown id: `404`.

#### `HEAD /v1/files/{id}`

The metadata face of the download (status and headers, no body).

#### `DELETE /v1/files/{id}`

Delete a record you own. The blob is never unlinked here: the reference
count drops, and the garbage collector unlinks the file only after its
grace period. `204` on success.

#### `POST /v1/files/{id}/send`

Deliver a record into another principal's inbox — the server-side copy:
the recipient gets a new record id pointing at the same blob, landed in
their space (`inbox/`). Request carries exactly one target (both or
neither is `400`):

```json
{ "to_user": "<user id>" }
```

or `{ "to_tagma": "<tagma id>" }`. `201` returns:

```json
{ "record_id": "<uuid>", "blob_id": "<sha-256>", "path": "/users/u1/inbox/report.pdf" }
```

Policy stays server-side: same-space requirement, the Tagma-to-User
refusal, and the landing path are ACL decisions the CLI only surfaces.
Every accepted delivery appends a row to the delivery log (below).

#### `GET /v1/files/admin/delivery-events?blob_id=&limit=`

Admin-only: the delivery log, newest first. Optional `blob_id` filter,
`limit` page cap. Each event carries `id`, `happened_at`, `from_principal`,
`to_principal`, `blob_id`, and the source/target record ids.

### CLI

The `kallip file` family (put/get/send/ls) is a thin face over these
routes; credentials ride the environment (`KALLIP_POLIS_URL`, whose
`/v1/files` derivation is the CLI's base URL, +
`KALLIP_FILES_TOKEN`), never CLI flags. Server-side callers
authenticate as themselves: the tagma presents its registered
enrollment credential for record media fetches. See
[kallip.md](kallip.md).

Source: [`crates/platform/kallip-files/src/`](https://github.com/ImitationGameLabs/kallipai/tree/main/crates/platform/kallip-files/src/).
