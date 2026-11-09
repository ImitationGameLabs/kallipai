//! OpenTelemetry tracing bootstrap for the polis services.
//!
//! One entry point, [`init_service_telemetry`], sits on top of the logging
//! bootstrap. With `OTEL_EXPORTER_OTLP_ENDPOINT` unset or blank it installs
//! exactly the subscriber `logging::init_service_logging` installs --
//! not a disabled exporter, but the same construction path, so a deployment
//! that never sets the variable cannot tell the two apart. With the variable
//! set it adds an OTLP/HTTP span exporter and the layer that feeds it.
//!
//! The module comes in one file per concern. [`spans`] opens a span on either
//! side of a call and puts the context on the wire; it needs no collector.
//! [`export`] holds the configuration, the exporter and the bootstrap, and is
//! compiled only under the `telemetry` feature. `server` is the axum middleware
//! that the [`spans`] module leaves to the framework, and is compiled only under
//! the `axum` feature. A process that only makes calls (a CLI, a daemon's own
//! client) enables `propagation` and keeps an [`HttpClient`] in the field it
//! sends its requests from.
//!
//! A span records a piece of work rather than a call, so the two sides of a
//! call are not the only places for one. A detached task (`tokio::spawn`)
//! inherits no scope: work that has to stay in the trace opens its span inside
//! the task or instruments the task's future ([`spans::background_span`]),
//! while a timer that only wakes a waiter carries no context to hand down and
//! spawns bare.
//!
//! The exporter never sits in a request path: spans go through a batch
//! processor whose queue is bounded, and a full queue drops spans instead of
//! blocking the caller. The queue drains on a thread with its own runtime, so
//! a collector that is down costs spans, never latency. The first failed
//! export says so once on the log channel and then stays quiet.
//!
//! A span dropped at a full queue leaves no trace of its own: the SDK counts
//! those drops but reports them only through its internal logs, which this
//! crate leaves off, so that loss is read from the collector's intake and
//! not from here. A failed export is the other case: [`export_stats`] counts
//! it and stamps it, which is what the health face reads. The
//! export layer carries its own filter, so a quiet `RUST_LOG` takes neither the
//! span nor the `traceparent` it injects: the chain stays whole at any log
//! level.
//!
//! A setup that cannot be built at all (a malformed endpoint, say) degrades:
//! one stderr line, tracing stays local, the service starts. Telemetry is not
//! load-bearing the way an event file channel is, so it does not take the
//! process down the way a broken `KALLIPAI_<SERVICE>_LOG_DIR` does.

#[cfg(feature = "telemetry")]
pub mod export;
#[cfg(feature = "axum")]
pub mod server;
pub mod spans;

pub use crate::SPAN_TARGET;
#[cfg(feature = "telemetry")]
pub use export::{
    Config, ENDPOINT_VAR, ExportSnapshot, ExportStats, Guard, SAMPLER_ARG_VAR, SAMPLER_VAR,
    SamplerSpec, config_from_env, config_from_lookup, export_layer, export_layer_authorized,
    export_stats, init_service_telemetry,
};
#[cfg(feature = "axum")]
pub use server::{server_span, with_server_span, with_server_span_except};
pub use spans::{
    HttpClient, Settle, background_span, http_client, inject_current_context, record_attribute,
    record_response, request_span,
};

// The tests reach the halves' internals through this module, so this is the
// only place that has to name them.
#[cfg(all(test, feature = "telemetry"))]
pub(crate) use export::{
    CountingExporter, HintOnceExporter, service_layers, service_layers_with_exporter,
    traces_endpoint,
};
#[cfg(all(feature = "telemetry", any(test, feature = "test-support")))]
pub(crate) use export::{RESOURCE_NAMESPACE, span_filter};
#[cfg(test)]
pub(crate) use spans::{
    ABANDONED, CANCELLED, FAILED, OK, URL_PATH_LIMIT, capped, parent_from_headers,
};

#[cfg(all(test, feature = "telemetry"))]
mod tests;

/// The export boundary, for tests: spans leave through a recorder kept in
/// memory instead of a collector, and a test reads them back as plain values.
///
/// Present in this crate's own tests and in a dependent crate whose
/// dev-dependencies turn on `test-support`; a build that enables neither
/// does not contain it. It is not a service API.
#[cfg(any(all(test, feature = "telemetry"), feature = "test-support"))]
pub mod test_observability;
