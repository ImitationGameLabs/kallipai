---
title: lesche HTTP API
description: The lesche data plane's public edge on the platform origin (repo-internal).
order: 46
internal: true
---

Both services are addressed on the single platform origin: the web app and
the tagma call `https://api.<d>/v1/archeion` and `https://api.<d>/v1/lesche`
(e.g. `api.kallipai.lan` in dev, `api.kallipai.com` in prod). Same-origin by
construction, the session cookie stays host-only (no `Domain` attribute),
and each service's CORS allowlist authorizes the app origin with
credentials.

## Archeion / Lesche Service-To-Service Boundary

The cloud relay is split into two services: the **archeion** (control plane:
identity, WebAuthn, tagma lifecycle, the durable identity Postgres store) and
the **lesche** (data plane: tagma relay tunnels, app event streams, envelope
routing, presence, plus the durable chat store (rooms, membership, message
payloads) in its own Postgres). The lesche's in-memory surfaces
are soft-state; its chat schema persists. It authenticates requests, resolves
tagma metadata, attests identity facts, and advances the tunnel-proof replay
guard through a narrow `ControlPlane` trait, reached over the archeion's non-public
`/internal/*` HTTP API.

That `/internal/*` surface is guarded by a shared-secret bearer
(the archeion-provisioned internal token, the same value on the archeion and the
lesche). The comparison is constant-time. If the token is
unset on the archeion, the `/internal` nest is not mounted at all (the archeion runs
standalone, no relay connected). The surface must be network-isolated so only
the lesche can reach it.

**Revocation latency**: the lesche verifies credentials per request against the
archeion (no auth cache). Its hot paths are long-lived connections (a tagma relay
tunnel, an app SSE stream) that authenticate once at open and are not
re-verified mid-stream, so revoking a tagma or disabling a user takes effect
on the lesche when the affected connection is next (re)established, not
necessarily the instant the archeion row changes. To force immediate
re-verification, drop the connection (the tagma relay reconnects; the app
reconnects). This is the v1 revocation contract; a JWT migration (local
validation, zero per-request RPC) is the future step if tighter coupling is
ever needed.
