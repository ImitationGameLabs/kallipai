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
  control-plane calls;
- the tagma's model request to the model gateway: the client library owns that
  transport, so the backend wrapper injects the context in its `send` instead
  of a `telemetry::http_client` call.

A client built by hand carries nothing, with one exception: the model request
is prepared by the client library on a transport it owns, and the backend
wrapper injects the context in its `send`. A process that never assembled the
exporter injects nothing either, because the propagator is published with it
(see [Known gaps](#known-gaps)).

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

A chat span has one more thing under it that this process does not own: the
model-gateway's request span for the same call. The request leaves with the
chat span's `traceparent`, so the gateway continues this trace instead of
starting one. A process that runs without an exporter injects nothing, and the
gateway then starts a trace of its own. A deployment that points the tagma at a
provider directly has no gateway in the path, so there is no span there at all.

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
`tool_timeout`; a turn that failed carries `turn_failed`.

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

When the gateway is instrumented too, its request span hangs under the chat
span, and the chat span's duration splits there: the gateway span is its own
work plus the wait on the model, the rest is this process's connection and
queueing.

## The metrics face: an OTLP push and `/health`

A trace answers "why was this request slow"; the metrics answer "how many,
how often, how long, and is a loop still ticking". They leave the process
instead of being read from it: with `OTEL_EXPORTER_OTLP_ENDPOINT` set, the
process posts to `<base>/v1/metrics` once a minute, onto the resource the spans
carry and with the same bearer token; with the variable unset it records into
the SDK's no-op meter, so nothing leaves. There is no metrics route: the
numbers reach a store through the collector, and they arrive carrying the
resource attributes that name the instance, so a store labels them without a
scrape config of its own.

The reader is periodic, so no observation sits in a request path: a value
lands in an aggregator the SDK owns and leaves on the next cycle, and the
guard's shutdown flushes what is queued. Temporality is spelled out as
cumulative, which is what a Prometheus store expects. A push to a collector
that is down, or that refuses the token, costs one cycle and one log line
(once per reason) and never latency, and the failure reaches the next
successful push as `telemetry_export_failed_total{reason}`.

`/health` does sit on the API listener (no second port), unauthenticated by
design and therefore **loopback only**: a peer that is not `127.0.0.1`/`::1`,
or a caller whose peer address cannot be established, gets 403.
`--health=false` (the same switch reads `KALLIPAI_TAGMA_HEALTH`) leaves the
route unmounted, which reads as 404.

The gate is a peer-address test, and that shapes two deployments. A probe
from another container or namespace arrives with that container's address,
so it is refused: probe from the same network namespace, or run the checker
inside it. A reverse proxy on the same machine connects
from `127.0.0.1`, so what it forwards reaches the endpoint; putting a
proxy in front of the API widens who can read it to everyone the proxy
admits.

The instruments fall in three groups:

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
  `relay_reconnects_total` already describe it. The 1:1 conversation envelope
  POST and the upstream batch are calls like any other, so a failed one shows
  up as an `error` on the `lesche` leg. Room and direct-session delivery are
  not counted: their failures reach the caller as an error, but no leg
  counter records them and no log line names them.
  `build_info{version,tagma_registration}`
  carries the enrolled registration id (`none` before enrollment, so enrolling
  changes a label value and never the series shape). It sits under a label of
  its own because `tagma_id` names the sending process: one label carrying two
  value spaces would arrive as a single joined `a;b` value.
  `/health` is exempt from `http_requests_total` and the request histogram: a
  probe measures the process, and counting probes would let the prober's
  cadence dominate. The exemption is by path rather than by peer, so a probe
  refused at the loopback gate leaves no series either; its only trace is a
  debug log line. The endpoint opens no span either: it is answered outside
  the server span, so a probe is not an entry in the traces it sits beside.
  The push itself is not counted: it rides the exporter's own client rather
  than `telemetry::http_client`, so no `outbound_requests_total` leg answers
  for it.
  `relay_dropped_total{reason}` counts inbound frames on the 1:1 op path that
  the tagma discarded because the two ends disagree: `decrypt`, `decode` or
  `unknown_op`. The
  family is absent until a drop happens, and healthy traffic never creates
  it, because a replay or an envelope arriving before the key exchange is
  not a disagreement.
- **Tallies another owner keeps**, read at collection time by an observable callback
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

The families, named as a store shows them, with the label names the code uses:

| Family | Type | Labels | Reported by | What it measures |
| --- | --- | --- | --- | --- |
| `kallipai_tagma_http_requests_total` | counter | `route`, `method`, `status_class` | request path | one count per response |
| `kallipai_tagma_http_request_duration_seconds` | histogram | `route` | request path | received to response finished |
| `kallipai_tagma_relay_reconnects_total` | counter | none | relay tunnel | sessions that ended and were reopened |
| `kallipai_tagma_relay_dropped_total` | counter | `reason` | relay tunnel | inbound frames the tagma discarded, by cause; drops that are normal protocol behaviour are not counted |
| `kallipai_tagma_relays_configured` | gauge | none | relay tunnel | relay entries the operator configured; 0 is a local-only tagma |
| `kallipai_tagma_relays_connected` | gauge | none | relay tunnel | entries whose tunnel is open right now |
| `kallipai_tagma_outbound_requests_total` | counter | `leg`, `outcome` | outbound calls | one count per platform call; a stream that stays open is not a call |
| `kallipai_tagma_outbound_request_duration_seconds` | histogram | `leg` | outbound calls | platform call duration |
| `kallipai_tagma_agents` | gauge | `state`: `live`, `faulted`, `off_duty` | roster | roster entries by registry state, plus how many of them are off duty; the off-duty count cuts the same set, so the three do not add up |
| `kallipai_tagma_build_info` | gauge | `version`, `registration` | boot | always 1; the labels name the build and the stored registration, the second absent until an enrollment writes one |
| `kallipai_tagma_background_task_age_seconds` | gauge | `task`: `history_gc`, `engine` | background loops | seconds since each loop last started a pass; `engine` is the loop `/health` reports as `work_schedule_engine` |
| `kallipai_tagma_telemetry_export_enabled` | gauge | none | export | 1 when an export layer is installed |
| `kallipai_tagma_telemetry_exported_total` | counter | none | export | spans the collector accepted |
| `kallipai_tagma_telemetry_export_failed_total` | counter | `reason`: `timeout`, `unavailable`, `unauthorized`, `other` | export | failed batches; `unauthorized` means the intake answered 401 or 403, so the token this process sends is not the one the intake accepts |
| `kallipai_tagma_telemetry_last_export_timestamp` | gauge | none | export | Unix seconds of the last accepted export, 0 while none has landed |
| `kallipai_tagma_bus_events_total` | counter | `topic` | bus | events published per topic |
| `kallipai_tagma_bus_lagged_total` | counter | `topic` | bus | events a subscriber missed |
| `kallipai_tagma_agent_wakes_total` | counter | `outcome` | adk sink | finished wakes |
| `kallipai_tagma_agent_wake_duration_seconds` | histogram | `outcome` | adk sink | how long a wake ran |
| `kallipai_tagma_model_requests_total` | counter | `outcome` | adk sink | send attempts |
| `kallipai_tagma_model_request_duration_seconds` | histogram | none | adk sink | one attempt, from its send to its end, every outcome together |
| `kallipai_tagma_model_first_token_seconds` | histogram | none | adk sink | send to the first streamed delta |
| `kallipai_tagma_model_tokens_total` | counter | `kind` | adk sink | tokens as the provider reported them |
| `kallipai_tagma_model_in_flight` | gauge | none | adk sink | requests in flight right now |
| `kallipai_tagma_tool_calls_total` | counter | `outcome` | adk sink | tool calls |
| `kallipai_tagma_tool_call_duration_seconds` | histogram | `outcome` | adk sink | how long a tool call ran |

Every family the process records is in this table: 26 instruments, and a
series the process reports that is absent here is a gap in the table. The
`Reported by` column names where the value is recorded or owned, so a family that
reads wrong can be traced to the code path that records it.

The process names a family in dots (`kallipai_tagma.http_requests`), and the
remote write renders that name with underscores on the way into a store, so
the table's spellings are the store's. Session, room and message totals stay
the platform's to count, so this face never disagrees with the lesche about
them.

`/health` answers `ok`, `degraded` or `not_ready`, with one entry per check.
Only `not_ready` (503) means "take this instance out of service", and it is
reserved for what the process cannot serve without: the `history_store`
handle, and a `profile_bundle` with neither sets nor a default. A configured
relay with no open tunnel, a stopped work-schedule engine, or a late background
loop is `degraded` (200): removing the instance would not fix a dead relay, and
the endpoint exists to make the fault visible rather than to blank a dashboard.
A loop that is on time with a failed last pass degrades the same way, and
`background_loops.failed` names it. `telemetry_export` degrades when an export
layer is installed and its last attempt failed with no success after it; an
uninstalled layer is ok, and so is a process that has exported nothing, because
a quiet tagma has no failure to report. The `history_store` check is a handle
check: it never queries the file behind the handle, so a store whose disk went
away keeps reading ready until the first query that needs it fails.

## Switches and cost

| Variable | Effect |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | the collector's OTLP base URL; a base URL gains `/v1/traces` or `/v1/metrics`. Unset or blank installs neither pipeline: tracing stays local, every instrument records into the no-op meter, and nothing leaves the process |
| `OTEL_EXPORTER_OTLP_HEADERS` | the generic header list both OTLP exporters read, for a service that has no credential of ours or whose credential the intake does not accept. `authorization` set here replaces the token a service passes, so a deployment sets one or the other, never both. The per-signal `OTEL_EXPORTER_OTLP_TRACES_HEADERS` takes precedence over this one |
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
or when full. One export call meets three bounds: the HTTP client bounds a
single attempt, the processor's 3 second budget bounds the whole call with
its retries, and the exporter stops itself 250 ms earlier, where the tally
also stops, so a collector that stops answering is counted and logged
instead of being cancelled unread. Shutdown flushes what is queued.

The span spend is per request and per call it makes — one forwarded request is
a handful of spans — so the sampler is the knob for span cost, not the batch
sizes. The metric spend is one push a minute per process whatever the traffic,
so its interval is the knob there.

On the stack side, `KALLIPAI_TELEMETRY_INTAKE_TOKEN_FILE` names the token file
mounted into the collector, and the compose files refuse to start without it.

## The intake: one door

One OTLP receiver serves the whole stack, on `4318` (HTTP) and `4317` (gRPC)
inside the network, behind a bearer-token authenticator: a push without the
token, or with another one, is refused at the receiver and never reaches a
backend. The token is a file, named by
`KALLIPAI_TELEMETRY_INTAKE_TOKEN_FILE` and mounted read-only at
`/etc/kallipai/intake-token`; the collector reads it when it starts, so
rotating it means restarting the collector. There is one door for the stack
rather than one per instance: the credential is the tenant, and the resource
attributes a sender contributes name the rest.

Two pipelines leave the receiver. Traces go to tempo over OTLP. Metrics go to
Prometheus over the remote-write protocol with
`resource_to_telemetry_conversion`, which turns `tagma.id` and
`service.instance.id` into labels; without it they would live on `target_info`
alone and every instance's numbers would arrive looking alike. Prometheus
publishes no host port in any compose file: its receiver takes writes without
a credential, so it stays on the compose network and Grafana is the way in.
Tempo publishes `3200` in the dev override and nothing in production.

The credential a process pushes with is the one it stores, and the tagma's is
the relay registration token at
`<dataRoot>/credentials/<entry>/tagma.token`. One intake token means the file
holds the token of the processes that push: where a service carries a
different credential, or none, `OTEL_EXPORTER_OTLP_HEADERS=authorization=Bearer
<token>` names the one the intake accepts. An exporter is built once, at
process start: a process that booted before its registration sends no
`authorization` header for its whole life (the resource attribute still says
`none`, which is a placeholder and not a credential) and shows up as
`unauthorized` until it restarts.

A refused credential is counted where the sender sees it:
`kallipai_tagma_telemetry_export_failed_total{reason="unauthorized"}`. The
collector neither counts an auth refusal (it is rejected before intake) nor
exports metrics of its own here, which is why the reason lives on the sender's
side.

A tagma that runs under the platform polis has no telemetry egress: the
endpoint tagma answers its agents and the platform, and every span and metric
it records stays in the process, so an investigation there reads the log and
`kallipctl` rather than a dashboard. Pointing `OTEL_EXPORTER_OTLP_ENDPOINT` at
a stack of your own turns the export on. Doing without it is a trade-off this
deployment shape pays for.

The NixOS form has no collector and no authentication: the services push
straight to tempo on `127.0.0.1`, and the network is the boundary. The
container form runs collector `0.164`, while nixpkgs still ships `0.155`;
`scripts/obs-acceptance.sh` validates every rendered text with the pinned
image before it starts anything, so a text that parses on one of them and not
the other fails there rather than at boot. That script is also the standing
proof of the path: it starts two tagma processes against a real collector,
tempo and Prometheus, and checks that a foreign or empty token is refused,
that one push carrying `tagma.id` lands in both stores, that counting the
instances by `tagma_id` and by `tagma_id` together with
`service_instance_id` answers the same number, that the request counter
grows across two export cycles, and that each instance's traces carry both
identity keys.

## Retention

`kallipai-telemetry-prune` prints what it would delete; `--apply` deletes and
reports every skip. Its budgets are 5 GiB and 72 hours, overridable with
`KALLIPAI_TELEMETRY_MAX_BYTES` and `KALLIPAI_TELEMETRY_MAX_AGE_HOURS`.

## Known gaps

- A NixOS host wires no metrics store for its services: the endpoint names
  tempo's OTLP receiver, which keeps traces, and the host Prometheus scrapes
  rather than receives. A tagma's minute push therefore has nowhere to land
  there, and its numbers stay in the process. The container form is the one
  with a collector standing in front of both stores.
- The model request now carries this process's trace context: the backend is
  wrapped so its send injects the span the request runs under, and the
  gateway's request span hangs under the chat span. On the leg the gateway
  forwards, the caller's `traceparent` travels on unchanged (see below), so a
  provider that honours the header joins this trace and one that ignores it
  leaves no span behind.
- The summarize call a compaction makes opens no chat span: it builds its
  request against the client outside the acquisition loop, so its tokens and
  its failures land on the turn's span and in the log only. Its request still
  goes through the wrapped backend, so it carries the context of the turn's
  span and the gateway continues the trace there.
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
  its request span). The metric half has an assertion against real stores:
  `scripts/obs-acceptance.sh` runs two tagma processes into a collector, tempo
  and Prometheus.
- Propagation lives only in a process that assembled the exporter: the global
  propagator is published with it, so a process that never sets
  `OTEL_EXPORTER_OTLP_ENDPOINT`, or never runs the bootstrap at all, injects
  no `traceparent`, however its clients are typed.
- The archeion OAuth exchange carries no trace context: it is the one
  outbound call whose header set is kept to the protocol's own, so its
  duration is on the log line only.
