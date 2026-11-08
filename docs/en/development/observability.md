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
The first-party calls are the ones that carry it on: an outbound request
whose client came from `telemetry::http_client` opens a client span and, in
a process that has published the propagator, injects `traceparent`, so the
service that answers continues this trace instead of starting a second one.
Those legs are:

- the tagma fetching a record from the files service, and the profile
  distribution face it pulls through;
- the relay client posting an envelope, and the tunnel client opening its
  stream;
- the archeion client behind enrol and health: in the tagma, in the files
  service's credential checks, in the admin console and in `kallipctl`;
- the tagma client shared by agents, the CLI and the cron daemon, and the
  CLI's own file client;
- the cron CLI calling its daemon, and the keryx and instances
  control-plane calls.

A client built by hand carries nothing, and a process that never assembled
the exporter injects nothing either, because the propagator is published
with it (see [Known gaps](#known-gaps)).

Five processes make first-party calls and assemble no export layer, so
their calls carry the type's promise and no header yet: `kallip`,
`kallipctl`, `kallipai-admin`, `kallipai-cron` and `kallipai-run` (see
[Known gaps](#known-gaps)).

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

## The client span's attributes

An outbound call through `telemetry::http_client` opens a span named for its
method alone (`GET`, `POST`) with `otel.kind=client`, and records where the
call went as attributes. This is the server rule's other face: a client call
has no route template to aggregate on, and a path with an id in it would
scatter one endpoint across a thousand names, so the name is a bucket and the
target is an attribute. Those attributes are the call and nothing else:

| Attribute | Value |
| --- | --- |
| `http.request.method` | the request method |
| `url.path` | the path, capped at 256 characters |
| `server.address` | the host the URL names, as written (never a resolved address) |
| `server.port` | the port the call went to, the scheme's default when the URL omits it |
| `http.response.status_code` | the status the service answered |
| `error.type` and a failed status | a 4xx or 5xx from the service; `timeout`, `transport` or `middleware` for a call that got no response |

The name also reaches the log prefix, so a line logged inside a client call
reads `request{otel.name=GET otel.kind=client}`: the method, not the endpoint,
which is on the span's attributes where the trace reads it.

The span ends when the response head exists: a body that fails afterwards is
the caller's error, and a streamed answer is recorded as the call that
started it. A call made outside every span roots a new trace, which is the
honest shape for work no request asked for.

The gateway adds three attributes of its own. `gateway.outcome` is on every
request routed to a generation endpoint, a refusal at a gate included.
`upstream.authority` and `upstream.api_family` come from the selection, so a
request a gate refuses carries neither: it ends before a credential is
selected. None of the three is on a request that was not routed to a generation
endpoint: not a `/health` probe, not a management path, not an unmatched
path, not a preflight.

| Attribute | Meaning |
| --- | --- |
| `gateway.outcome` | how the request ended, one value per such request: `gated_auth`, `gated_route`, `gated_visibility`, `gated_payload`, `upstream_2xx`, `upstream_4xx`, `upstream_5xx`, `upstream_other`, `connect_error`, `resolve_error`, `upstream_error`, `downstream_error`, `internal_error` |
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

## Inside an agent: the wake tree

The services answer under one request span. An agent does not work that way: a
wake runs turns until something ends it, and every turn may send to a model
and call tools before it ends. Its trace has a shape of its own.

```text
invoke_agent <role>            one wake
└── turn                      one turn, kallipai.turn.index
    ├── chat <model>          one request to a model endpoint
    └── execute_tool <tool>   one tool call
```

These spans belong to the `tagma` process (`service.name=tagma`), not to one of
the services above. Every one of them is created on the `kallipai.spans`
target, so the export layer records it whatever `RUST_LOG` says. Names stay low
cardinality: a role, a model or a tool name may appear in them, while ids,
indices and outcomes travel as attributes.

| Span | Name (`otel.name`) | Attributes |
| --- | --- | --- |
| wake | `invoke_agent <role>` | `gen_ai.operation.name`, `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id`, `kallipai.wake.origin`, `kallipai.wake.outcome` |
| turn | `turn` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id`, `kallipai.turn.index`, `kallipai.turn.messages`, `kallipai.turn.prompt_tokens`, `kallipai.turn.outcome` |
| model request | `chat <model>` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.agent.id`, `gen_ai.conversation.id`, `kallipai.profile.id`, `kallipai.chat.attempts`, the `gen_ai.usage.*` counters |
| tool call | `execute_tool <tool>` | `gen_ai.operation.name`, `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.agent.id`, `gen_ai.conversation.id`, `kallipai.tool.outcome` |

`kallipai.wake.origin` is `external` or `system`: the two ways a wake starts.

`kallipai.wake.outcome` says what the wake left behind: `awake` when nothing
holds the loop, `held` when a park, a re-checking gate or an armed backoff does,
`terminated` when a lifecycle cancel ended the run.

`kallipai.turn.index` counts turns from zero within the wake.
`kallipai.turn.outcome` says how the turn ended: `break` when the agent said it
was done, `bare` for a turn that answered with no tool call (the loop
re-enters), `park` for a turn that ended parked, `recompose` when the turn
rebuilt the context before going on, `continue` when the loop simply went on,
`cancelled` when the lifecycle stopped the turn, and `error` for a turn that
failed.

`kallipai.chat.attempts` counts the sends one request made. A send the retry
shell repeats stays in the same span, and every send overwrites the count, so
the span ends holding the total. A request issued again from the top of the
acquisition loop opens a span of its own: that is the request after a
mid-stream failure, and the first request on each new profile a failover
reaches. One turn can therefore hold several chat spans, and a slow turn
reads as the sum of them plus the tool calls between.

The usage counters tell what the response cost: `gen_ai.usage.input_tokens` and
`gen_ai.usage.output_tokens` always, `gen_ai.usage.cache_read.input_tokens` and
`gen_ai.usage.cache_write.input_tokens` only when the provider reported them. A
counter the provider said nothing about stays absent rather than reading as
zero, because "no cache was used" and "the provider does not report cache" are
different facts about a request.

`kallipai.tool.outcome` names how a call ended: `success`, `failed`, `timeout`,
`deferred`, `cancelled` or `skipped`. `skipped` marks a call the loop dropped
because an earlier call stopped it; `deferred` one the tool moved to the
background; both settle as `ok`, since neither is this call's fault. `timeout`
is the run loop's own deadline expiring on the call, kept apart from a tool that
reports a failure of its own.

### How an agent span ends

Every agent span carries `kallipai.outcome`: `ok`, `failed`, `cancelled` or
`abandoned`. A span that failed also carries `error.type` and a failed status
holding the same word, so a trace filters on either. The two axes answer
different questions, and `cancelled` is why they are separate: a daemon shutting
down or a task being removed ends work that did not go wrong, so it keeps an
unset status and reads as cancelled by its outcome alone. Counting a restart as
an error would drown the failures a trace is read for.

`abandoned` is the fallback for a scope that ended without a verdict, an early
return or a panic: without it such a scope would read as a success. Failure
codes are a closed set of lowercase words naming a class, never a message. The
model and stream failures use the retry classifier's own codes (`transient`,
`quota_blocked`, `throttled`, `endpoint_permanent`, `request_permanent`,
`local_config`, `response_shape`, `unclassified`); a tool uses `tool_failed` or
`tool_timeout`; a turn that failed carries `round_failed`.

The prose stays in the log line, and only there: no event reaches the trace at
any level, so a message or an upstream error body cannot ride along as one. No
attribute carries a prompt, a tool's arguments or result, a model's answer or a
credential either; tests plant a sentinel in the conversation and check that no
attribute of the trace contains it, and another plants one in an `error!` and
checks that the exported span has no events.

### Reading an agent's trace

The agent's spans are the ones with a `turn` beneath them, so the wake's own
`invoke_agent <role>` name is the entry point: search by it, or by
`service.name=tagma` and the attribute `kallipai.turn.outcome=error` to find the
turns that failed first. The turn's span is where a wake's duration is
explained: the chat spans under it are the time spent waiting on a model, the
tool spans the time spent in this process.

## The scrape face: `/metrics` and `/health`

A trace answers "why was this request slow"; the scrape face answers "how many,
how often, how long, and is a loop still ticking". Both endpoints ride the API
listener (no second port), both are unauthenticated by design and therefore
**loopback only**: a peer that is not `127.0.0.1`/`::1`, or a caller whose peer
address cannot be established, gets 403. `--metrics=false` and `--health=false`
(the same switches read `KALLIPAI_TAGMA_METRICS` and `KALLIPAI_TAGMA_HEALTH`)
leave the route unmounted, which reads as 404.

The gate is a peer-address test, and that shapes two deployments. A scrape
from another container or namespace arrives with that container's address,
so it is refused: scrape from the same network namespace, or run the
collector's agent inside it. A reverse proxy on the same machine connects
from `127.0.0.1`, so what it forwards reaches both endpoints; putting a
proxy in front of the API widens who can read them to everyone the proxy
admits.

`/metrics` renders the Prometheus text format from three groups of families:

- **This crate's own counters.** `kallipai_tagma_http_requests_total{route,
  method,status_class}` and `..._http_request_duration_seconds{route}`: the
  route is the matched template, so an id-bearing path collapses into one
  series, and a request that matched nothing is `route="unmatched"`. `method`
  is one of the methods this listener answers, or `other`: the timing layer
  runs outside the token check, so the value must not be peer-controlled. The relay
  gauges (`relays_configured`, `relays_connected`, `relay_reconnects_total`),
  the outbound client counters (`outbound_requests_total{leg,outcome}` and
  `outbound_request_duration_seconds{leg}`, with legs `files`, `profiles`,
  `lesche` and `archeion`). A leg counts calls: opening a stream is one, and a
  tunnel that then stays open is not, because `relays_connected` and
  `relay_reconnects_total` already describe it. `build_info{version,tagma_id}`
  carries the enrolled id (`none` before enrollment, so enrolling changes a
  label value and never the series shape), and the same id is `tagma.id` on the
  exporter's resource: one value, dotted in the trace, underscored in the
  scrape label. Both observability endpoints are exempt from
  `http_requests_total` and the request histogram: a scrape measures the
  process, and counting scrapes would let a scraper's cadence dominate. The
  exemption is by path rather than by peer, so a scrape refused at the loopback
  gate leaves no series either; its only trace is a debug log line. Neither
  endpoint opens a span either: the two paths are answered outside the server
  span, so a scrape is not an entry in the traces it sits beside.
- **Tallies another owner keeps**, encoded on each gather by a collector
  instead of mirrored into a gauge: the agent loop's wakes, one count per
  finished wake, from its entry to the point where it hands control back, and
  its model requests, first-token wait, tokens by kind, tool calls and requests
  in flight. A metric wake is the trace's `invoke_agent` span: the turns inside
  a wake, and the model and tool calls they make, stay in the trace and are
  not a family of their own. The label is `completed`, `failed` or
  `cancelled`: a wake the trace calls `held` always counts `failed`, and both
  an interrupt and a lifecycle cancellation count `cancelled`, so only the
  trace tells those two apart.
  A `failed` wake is one that did not finish its work, not an error: an armed
  backoff and a quota window park that way too, while a `break(wait)` ended
  its work and counts `completed`.
  A model counter and its duration move once per
  send attempt, timed from that send to the attempt's end, so a backoff
  between attempts never lands in a duration; the first-token wait is the
  adopted stream's own send. A send cancelled while it was still being
  connected is not counted at all: the cancellation lands either in a send or
  in a backoff between attempts, and the arm cannot tell which, so no ending
  of an attempt is known. Then the event
  bus's publishes and its dropped-to-lag counts per topic; the agent roster and
  how many agents are off duty; the export pipeline's accepted spans and its
  failed exports by reason. A monotone value is encoded as a counter, so
  `rate()` works on it; a value that goes up and down is a gauge.
- **Loop freshness.** `background_task_age_seconds{task}` for the chat-history
  GC and the work-schedule engine. A loop that has never beaten reports the
  process age, so a loop that died at boot looks fresh for its window and then
  turns late: an operator sees it once it is past its deadline, not at the
  moment it dies. A loop whose last pass failed is a separate fact, reported in
  `/health` rather than guessed from the age.

The families the collector encodes, with the label names the code uses:

| Family | Type | Labels | What it measures |
| --- | --- | --- | --- |
| `kallipai_tagma_agents` | gauge | `state`: `live`, `faulted`, `off_duty` | roster entries by registry state, plus how many of them are off duty; the off-duty count cuts the same set, so the three do not add up |
| `kallipai_tagma_agent_wakes_total` | counter | `outcome` | finished wakes |
| `kallipai_tagma_agent_wake_duration_seconds` | histogram | `outcome` | how long a wake ran |
| `kallipai_tagma_model_requests_total` | counter | `outcome` | send attempts |
| `kallipai_tagma_model_request_duration_seconds` | histogram | none | one attempt, from its send to its end |
| `kallipai_tagma_model_first_token_seconds` | histogram | none | send to the first streamed delta |
| `kallipai_tagma_model_tokens_total` | counter | `kind` | tokens as the provider reported them |
| `kallipai_tagma_model_in_flight` | gauge | none | requests in flight right now |
| `kallipai_tagma_tool_calls_total` | counter | `outcome` | tool calls |
| `kallipai_tagma_tool_call_duration_seconds` | histogram | `outcome` | how long a tool call ran |
| `kallipai_tagma_bus_events_total` | counter | `topic` | events published per topic |
| `kallipai_tagma_bus_lagged_total` | counter | `topic` | events a subscriber missed |
| `kallipai_tagma_telemetry_export_enabled` | gauge | none | 1 when an export layer is installed |
| `kallipai_tagma_telemetry_exported_total` | counter | none | spans the collector accepted |
| `kallipai_tagma_telemetry_export_failed_total` | counter | `reason`: `timeout`, `unavailable`, `other` | failed batches |
| `kallipai_tagma_telemetry_last_export_timestamp` | gauge | none | Unix seconds of the last accepted export, 0 while none has landed |
| `kallipai_tagma_background_task_age_seconds` | gauge | `task` | seconds since each loop last started a pass |

The same measurements appear in traces under different names: a scrape family
is `kallipai_tagma_*`, a span attribute is dotted (`gateway.outcome`). Session,
room and message totals stay the platform's to count, so this face never
disagrees with the lesche about them.

`/health` answers `ok`, `degraded` or `not_ready`, with one entry per check.
Only `not_ready` (503) means "take this instance out of service", and it is
reserved for what the process cannot serve without: the chat-history store
handle, and a profile bundle with neither sets nor a default. A configured relay
with no open tunnel, a stopped engine tick, or a late background loop is
`degraded` (200): removing the instance would not fix a dead relay, and the
endpoint exists to make the fault visible rather than to blank a dashboard.
A loop that is on time with a failed last pass degrades the same way, and
`background.failed` names it. The store check is a handle check: it never
queries the file behind the handle, so a store whose disk went away keeps
reading ready until the first query that needs it fails.

## Switches and cost

| Variable | Effect |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | the collector's OTLP base URL; a base URL gains `/v1/traces`. Unset or blank installs no exporter at all: tracing stays local and no span leaves the process |
| `OTEL_TRACES_SAMPLER` | `parentbased_always_on` (the default), `parentbased_always_off`, `parentbased_traceidratio`, `always_on`, `always_off`, `traceidratio` |
| `OTEL_TRACES_SAMPLER_ARG` | the ratio for the two `traceidratio` families; an unparseable or out-of-range value keeps the default |
| `RUST_LOG` | the log filter: what is printed, and, with no exporter installed, which span callsites are interested. Default `info`; the store spans need `trace`. Once an exporter is configured the export layer carries its own filter and records every span on the `kallipai.spans` target at `info`, so a quiet `RUST_LOG` no longer drops spans or the `traceparent` they inject; no log event reaches the trace at any level, and the prose stays in the log line. The split is by target, so `RUST_LOG=kallipai_common=info` no longer selects those spans by module path |

An instance the daemon starts receives the daemon's `OTEL_EXPORTER_OTLP_ENDPOINT`
and sampler values, plus `service.instance.id=<slug>` whenever its environment
carries the generic `OTEL_EXPORTER_OTLP_ENDPOINT`, from the daemon or from a
login shell; the per-signal endpoint keys are not read here. See [Local
daemon](../reference/environment-variables/daemon.md). A request cannot name
these keys.

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

- A model request carries no trace context onward: the request is sent inside
  the client library, which offers no hook to inject one, so what is missing is
  an upstream seam (an injectable client, or the send step made public), not a
  header this side declines to set. A provider's own spans therefore never join
  this trace.
- The summarize call a compaction makes opens no chat span: it builds its
  request against the client outside the acquisition loop, so its tokens and
  its failures land on the turn's span and in the log only.
- A wake and the delivery that triggered it are two traces: a trigger arrives
  through an asynchronous queue that carries no trace context, so a wake's root
  has no parent even when a message asked for it.
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
- Propagation lives only in a process that assembled the exporter: the global
  propagator is published with it, so a process that never sets
  `OTEL_EXPORTER_OTLP_ENDPOINT`, or never runs the bootstrap at all, injects
  no `traceparent`, however its clients are typed.
- The archeion OAuth exchange carries no trace context: it is the one
  outbound call whose header set is kept to the protocol's own, so its
  duration is on the log line only.
