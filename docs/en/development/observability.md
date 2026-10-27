---
title: Traces (OpenTelemetry)
description: What the services trace, which attributes a span carries, and how to read a trace.
order: 45
internal: true
---

Every service bootstraps the same tracing stack from `kallipai-common`: a
`tracing` subscriber and, when an OTLP endpoint is configured and the exporter
can be built, an OpenTelemetry layer over it whose batch exporter ships spans
to the collector. This page
covers what a trace looks like and how to read one. The stack's own bring-up
is in the [Archeion Side](setup.md#archeion-side) section of the setup page.

## One request, one span per hop

Both faces of a service answer under one request span:

- the **axum face** mounts `telemetry::with_server_span` on its router, so
  every handler runs inside a span named for the method and the route template;
- the **gateway data plane** opens the same span shape at the top of
  `request_filter`, before anything answers, so a preflight, a gate refusal, a
  health probe and a forwarded request are each one span.

Everything the request causes then hangs under that span:

- a call through `telemetry::http_client` leaves a client span: on the gateway
  that is the archeion identity check, made on both faces, and the management
  face's own internal lookups; the forwarded leg is proxied by pingora, which
  opens no client span of its own;
- the store work sea-orm emits (`execute`, `query_all`) hangs under this span
  too, at trace level.

A trace therefore reads as one request span with the calls and queries it
caused beneath it, in the order they happened.

The two faces end their span differently. The axum span ends when the handler
hands back the response head, so a streamed body runs on outside it and a body
that fails mid-stream is not recorded on it. The gateway's span is held by the
request context, which keeps it alive until the request is over, so a forwarded
body — a streamed answer included — is inside it, and the span reaches the
trace only at the end.

Spans carry no request body, no response body, no header and no credential.
The URI path travels as `url.path` (capped at 256 characters); the query
string, the headers and the payloads do not.

## The request span's attributes

| Attribute | axum face | Gateway data plane |
| --- | --- | --- |
| span name (`otel.name`) | `<method> <MatchedPath>` | `<method> <route label>` |
| `http.request.method` | the request method | the request method |
| `http.route` | the matched route template, `unmatched` when nothing matched | the entry path the request was routed to, bounded; `preflight` for a CORS probe |
| `url.path` | the path, capped at 256 characters | the path, capped at 256 characters |
| `http.response.status_code` | the status the handler returned | the status the client received |
| `error.type` and a failed status | a 5xx only | a 5xx only |

The gateway's `http.route` is the path the request was routed to: a forwarded
request carries the entry endpoint as it is, the management family collapses to
`/admin/*`, the health probe is `/health`, a path that routes nowhere is
`unmatched`, a known path with the wrong method is `method_not_allowed`, and a
CORS probe is `preflight`. `Route::label` writes those words except
`preflight`, which `request_filter` names.

The span name and `http.route` are bounded values, never the concrete path: an
id in a span name would scatter one endpoint across a thousand names, so the
path travels as an attribute instead. A 4xx records its status and stays
`unset`: for a server span the semantic conventions reserve the error status
for a 5xx.

The gateway adds three attributes of its own. `gateway.outcome` is on every
request routed to a generation endpoint, a refusal at a gate included.
`upstream.authority` and `upstream.api_family` come from the selection, so a
request a gate refuses carries neither: it ends before a credential is
selected. None of the three is on a request that was not routed to a generation
endpoint: not a `/health` probe, not a management path, not an unmatched
path, not a preflight.

| Attribute | Meaning |
| --- | --- |
| `gateway.outcome` | how the request ended, one value per such request: `gated_auth`, `gated_route`, `gated_visibility`, `gated_parking`, `gated_payload`, `upstream_2xx`, `upstream_4xx`, `upstream_5xx`, `upstream_other`, `connect_error`, `resolve_error`, `upstream_error`, `downstream_error`, `internal_error` |
| `upstream.authority` | the selected credential's URL authority, which is also the label its metrics carry: an address, host and port, never a key or a token |
| `upstream.api_family` | the registry family of the selected provider |

A request whose selected upstream does not resolve answers `503` with a
`retry-after` header in delay-seconds: the lookup failed before anything was
sent upstream, so the caller may retry, and the gateway counts the request as
`resolve_error` rather than as its own bug. That 503 is the shape to follow
for any other answer that means `dependency unavailable, retry`: one hint
header, and no resolver text in the body. The two faces then part ways. The
resolution counter labels the host it tried under `upstream`, an authority it
knows before it dials anything. The failure line has neither an `authority`
nor a `profile_id` field for a request that never held a selection; the host
appears there in the `error` text instead.

## Reading a trace

The dev stack exports to `otelcol` and stores in `tempo`; Grafana serves the UI
at `http://127.0.0.1:3001`. In Explore, pick the Tempo data source and search
by:

- `service.name`: each service registers the name its `main` passes at boot,
  so `archeion`, `model-gateway`, `keryx` and the others are what you search.
  Every service also reports `service.namespace=kallipai`;
- the span name (`POST /messages`, `GET /v1/files/{id}`), which is bounded and
  therefore worth aggregating on;
- an attribute: `http.route`, `gateway.outcome`, `error.type`.

Logs do not carry the trace id. A log line names the span it was written in —
`request{otel.name=POST /messages otel.kind=server}` — so a line and a span
are matched by those two fields and the timestamp.

The match is only as precise as those two fields: two requests to one route in
the same instant spell the same prefix, and only the timestamp tells them apart,
so a tight race is not separable this way.

## Switches and cost

| Variable | Effect |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | the collector's OTLP base URL; a base URL gains `/v1/traces`. Unset or blank installs no exporter at all: tracing stays local and no span leaves the process |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` (the default), `parentbased_always_off`, `parentbased_traceidratio`, `always_on`, `always_off`, `traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | the ratio for the two `traceidratio` families; an unparseable or out-of-range value keeps the default |
| `RUST_LOG` | one filter for both the events and the export layer: a span below it is never created, so it never reaches the trace. Default `info`; the store spans need `trace` |

The default sampler follows the parent, so a trace is sampled or dropped as a
whole across a hop rather than half-recorded. Batching bounds the cost: up to
2048 spans queued, exported in batches of 512, a batch leaving every 5 seconds
or when full, and each export call bounded at 3 seconds so a stalled collector
cannot pin the queue. Shutdown flushes what is queued.

The spend is per request and per call it makes — one forwarded request is a
handful of spans — so the sampler is the knob for cost, not the batch sizes.

## Retention

`kallipai-telemetry-prune` prints what it would delete; `--apply` deletes and
reports every skip. Its budgets are 5 GiB and 72 hours, overridable with
`KALLIPAI_TELEMETRY_MAX_BYTES` and `KALLIPAI_TELEMETRY_MAX_AGE_HOURS`.

## Known gaps

- The forwarded leg is proxied by pingora, which opens no client span: the
  gateway injects no `traceparent` into that leg and starts no span for it, so
  a provider joins this trace only when the caller's own `traceparent` reaches
  it.
- A CORS preflight is answered before the routing table and before a credential
  is selected, so on the data plane its span carries `http.route=preflight` and
  the status, but neither `gateway.outcome` nor an `upstream.*` attribute.
- The parent/child relation across faces is covered by tests (a caller's
  `traceparent` is adopted, and a rejected request's archeion checks hang under
  its request span); there is no automated assertion against a running
  collector.
