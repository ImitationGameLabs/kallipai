//! Span creation for both sides of a call: what a request answers under, and
//! what an outbound call opens.
//!
//! A server turns an inbound request into a span ([`request_span`]; the axum
//! middleware in the sibling `server` module calls it where the framework's
//! seam is available). A client turns an outbound call into a span and injects
//! the `traceparent` the next hop reads ([`HttpClient`], built by
//! [`http_client`]). Both halves share one propagator, published with the
//! exporter in the `export` module.
//!
//! The `HttpClient` alias is the seam every first-party call site stores:
//! because a bare `reqwest::Client` does not go into that field, whatever
//! leaves through it carries the current context, and a call site that wants
//! to drop the context has to spell the bypass out.
//!
//! A client crate names `reqwest-middleware` in its own manifest, and that is
//! not a leak: the seam's type is the middleware's `ClientWithMiddleware`, so
//! an override point takes a `RequestBuilder` from the same crate, and a call's
//! failure arrives as the middleware's error too. The dependency spells out
//! what the field's type already says.

use opentelemetry::propagation::Extractor;
use reqwest_middleware::{ClientWithMiddleware, Middleware, Next};

use crate::SPAN_TARGET;

/// The span kind a server request declares; a `Display` value renders into the
/// log prefix unquoted, which a string literal does not.
pub(crate) const SERVER_SPAN_KIND: &str = "server";

/// The parent an inbound request inherits: the W3C `traceparent` it carries,
/// or nothing at all when it carries none.
///
/// The extraction starts from an empty [`opentelemetry::Context`] rather than
/// from the ambient one. `extract`'s default start is `Context::current()`,
/// and the W3C propagator returns the context it was handed unchanged when the
/// carrier holds no valid `traceparent` -- so a request with no header would
/// adopt whatever span the serving task happened to be in, and a handler run
/// inside another span (a test stub in the caller's process, say) would look
/// parented with nothing on the wire. Starting empty makes "no header" mean a
/// new trace, which is what a request from outside the system is.
pub(crate) fn parent_from_headers(headers: &http::HeaderMap) -> opentelemetry::Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract_with_context(&opentelemetry::Context::new(), &HeaderCarrier(headers))
    })
}

/// The server span for one request: its name is the method and the route
/// label, its attributes are the OpenTelemetry HTTP ones.
///
/// The name deliberately avoids the raw path: an id in a span name scatters
/// one endpoint across a thousand names and defeats aggregation. The path
/// travels as `url.path` instead, capped at 256 characters, so a trace can
/// still say which room a call was for without a name nothing can aggregate.
///
/// Only `otel.name` and `otel.kind` are tracing fields: those two render into
/// the prefix of every event inside the request, and the rest is worth
/// nothing there. Everything else is a span attribute, which costs the log
/// no space.
///
/// Every span opened here carries the tracing name `request`, the client
/// span a call makes included (see [`http_client`]). `otel.kind` is what tells
/// two of them apart, and `otel.name` says more: a server span names the route
/// it matched, a client span the method it called. Both sit in the log prefix.
///
/// A span the OpenTelemetry layer does not own cannot take a parent; it then
/// stays a root, which is what a request without a caller's context is. When
/// the request carries a W3C `traceparent`, it becomes the span's parent,
/// so one trace spans the services a call crosses. The parent is read out of an
/// empty context (see `parent_from_headers`), so a request with no header
/// starts a new trace whatever its caller was doing. A request with no header
/// and a request with no caller each put the span at the root of a new trace.
pub fn request_span(
    method: &str,
    route: &str,
    path: &str,
    headers: &http::HeaderMap,
) -> tracing::Span {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let name = format!("{method} {route}");
    let span = tracing::info_span!(
        target: SPAN_TARGET,
        "request",
        otel.name = %name,
        otel.kind = %SERVER_SPAN_KIND,
    );
    // The parent travels with the request; adopting it before the span is
    // entered is what lets the OpenTelemetry layer build the span under it.
    // The result is dropped: with no OTLP endpoint set, no OpenTelemetry
    // layer is in the subscriber, so there is no span to parent and the call
    // reports `LayerNotFound` -- the ordinary case, not a fault. `AlreadyStarted`
    // is not expected either: the parent is set before the span enters a
    // context, and a span that has entered one cannot be reparented. Move that
    // order and the cross-service trace assertions fail.
    let _ = span.set_parent(parent_from_headers(headers));
    span.set_attribute("http.request.method", method.to_string());
    span.set_attribute("http.route", route.to_string());
    span.set_attribute("url.path", capped(path, URL_PATH_LIMIT).to_string());
    span
}

/// The status the response carried, recorded on the span that served it. A
/// server-side failure also names an error type and fails the status.
pub fn record_response(span: &tracing::Span, status: u16) {
    use opentelemetry::trace::Status;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    span.set_attribute("http.response.status_code", i64::from(status));
    if (500..600).contains(&status) {
        span.set_attribute("error.type", format!("http_{status}"));
        span.set_status(Status::error(status.to_string()));
    }
}

/// One attribute of a face's own vocabulary, on a span built elsewhere.
///
/// The callers do not depend on `tracing-opentelemetry`, and a span takes no
/// attribute without that trait in scope: this shell keeps the dependency
/// in this crate instead of in every crate that records one.
pub fn record_attribute(span: &tracing::Span, key: &str, value: impl Into<String>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    span.set_attribute(key.to_string(), value.into());
}

/// The span kind a background attempt declares: no caller waits on its answer.
pub(crate) const INTERNAL_SPAN_KIND: &str = "internal";

/// The span for one attempt of work that no request asked for.
///
/// `tokio::spawn` carries no scope: a task detached from whatever spawned it
/// starts outside the spawner's span, so a trace that should carry the
/// attempt has to start it here. It is a root by construction, not by the
/// call site's luck: `info_span!` takes whatever span the creating task is
/// in as the parent, so `parent: None` is what stops a request that reaches
/// this function from hanging the attempt under itself.
///
/// Entering it around the attempt (`Instrument::instrument`) is what puts the
/// client calls the attempt makes under it; created and dropped without being
/// entered, it carries the attempt's duration and nothing below it.
///
/// The name is one name per kind of attempt (`repull_profile_source`), so
/// every attempt of that kind aggregates into one series, the way a request
/// span aggregates on its route template.
pub fn background_span(name: &str) -> tracing::Span {
    tracing::info_span!(
        target: SPAN_TARGET,
        parent: None,
        "background",
        otel.name = %name,
        otel.kind = %INTERNAL_SPAN_KIND,
    )
}

/// The attribute a settled span carries: the one word for how its scope ended.
const OUTCOME: &str = "kallipai.outcome";
/// A scope that finished its work.
pub(crate) const OK: &str = "ok";
/// A scope that failed at something.
pub(crate) const FAILED: &str = "failed";
/// A scope that was cancelled from outside rather than failing.
pub(crate) const CANCELLED: &str = "cancelled";
/// A scope that ended without saying how it ended.
pub(crate) const ABANDONED: &str = "abandoned";

/// Settles how one span ended, once, from the scope that opened it.
///
/// A span is dropped when its scope ends, and a scope that ends without a
/// verdict -- an early `?`, a panic, a path nobody thought about -- would
/// otherwise read as a success in the trace. The guard writes the verdict, and
/// [`Drop`] writes `abandoned` for whatever the scope did not say.
///
/// The verdict has two axes, and they answer different questions.
/// `kallipai.outcome` names how the scope ended, on every span. A failing
/// status with `error.type` marks the ends where the work went wrong. A
/// cancelled scope is neither: a daemon shutting down, a task removed, a round
/// interrupted ends work that did not fail, so it keeps an unset status and
/// reads as cancelled by its outcome alone. Counting a restart as an error
/// would let operations drown out the failures the trace is read for.
///
/// Nothing here carries text: a code names a class (`tool_timeout`,
/// `quota_blocked`), never a message. The span's attributes and status are
/// what the trace keeps; the log line keeps the prose.
///
/// The guard lives as long as the scope: bind it (`let settle = ...`) and call
/// one of the verdicts on the way out. A guard dropped at the end of its own
/// statement settles the span early, which is why building one is `#[must_use]`.
#[must_use = "the guard must live as long as the scope it settles"]
pub struct Settle {
    span: tracing::Span,
    settled: bool,
}

impl Settle {
    /// Take responsibility for how `span` ends.
    pub fn new(span: &tracing::Span) -> Self {
        Self {
            span: span.clone(),
            settled: false,
        }
    }

    /// The scope finished its work.
    pub fn done(mut self) {
        self.outcome(OK);
    }

    /// The scope failed, and `code` names the class of the failure.
    pub fn failed(mut self, code: &str) {
        self.fail(FAILED, code);
    }

    /// The scope was cancelled from outside rather than failing.
    pub fn cancelled(mut self) {
        self.outcome(CANCELLED);
    }

    /// Write the outcome word, which every span gets.
    fn outcome(&mut self, word: &str) {
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        self.span
            .set_attribute(OUTCOME.to_string(), word.to_string());
        self.settled = true;
    }

    /// Write a failure: the outcome word, the class as `error.type`, and a
    /// failed status carrying the same class, so a trace filters on either.
    /// `outcome` and `code` differ only for the abandoned fallback, whose
    /// class is the same word as its verdict.
    fn fail(&mut self, outcome: &str, code: &str) {
        use opentelemetry::trace::Status;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        self.span
            .set_attribute(OUTCOME.to_string(), outcome.to_string());
        self.span.set_attribute("error.type", code.to_string());
        self.span.set_status(Status::error(code.to_string()));
        self.settled = true;
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.settled {
            self.fail(ABANDONED, ABANDONED);
        }
    }
}

/// The request headers as a propagator sees them.
struct HeaderCarrier<'a>(&'a http::HeaderMap);

impl Extractor for HeaderCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

/// Cap for `url.path`. A path is caller data, and a deeply nested one must not
/// grow every span it lands on; truncation keeps whole characters.
pub(crate) const URL_PATH_LIMIT: usize = 256;

pub(crate) fn capped(value: &str, limit: usize) -> &str {
    match value.char_indices().nth(limit) {
        Some((at, _)) => &value[..at],
        None => value,
    }
}

/// The span kind an outgoing call declares; a `Display` value renders into the
/// log prefix unquoted, which a string literal does not.
pub(crate) const CLIENT_SPAN_KIND: &str = "client";

/// The client a service stores for the calls it makes to another service.
///
/// The alias is the seam: these fields are built by [`http_client`], and a bare
/// `reqwest::Client` does not go into one. A bypass remains -- the middleware
/// client's `From`/`Default` -- but it takes spelling out, so dropping the
/// propagation is a decision a reader of the call site sees.
pub type HttpClient = ClientWithMiddleware;

/// Build a client whose every call belongs to a trace.
///
/// The builder is the caller's -- timeouts and pool settings stay where they
/// are -- and this adds the one middleware that propagates. A client that the
/// builder cannot build comes back as the builder's own error.
///
/// Each call opens a `client` span under the span the caller is in, or as the
/// root of a new trace when the caller is in none, and injects that span's
/// context into the request headers with the process-wide W3C propagator: the
/// service that answers continues the same trace instead of starting a second
/// one. The span is named for the method alone; the call's target is recorded
/// as `server.address`, `server.port` and `url.path`, with the response status.
///
/// A response that failed (4xx or 5xx) also records `error.type` as
/// `http_<code>` and sets an error status. A call that never got a response
/// records `error.type` alone and puts the same word in the status: `timeout`
/// for a call that ran out of time, `transport` for any other failure below
/// the middleware chain, `middleware` for a failure inside it. No header,
/// body, query or message text reaches a span.
///
/// The span ends when the response head exists, so a body that fails to arrive
/// or to decode afterwards is the caller's error: the span shows a call that
/// succeeded.
///
/// A request that never went on the wire -- a URL the builder refuses, a body
/// that will not serialize -- is named `transport` too: this vocabulary has no
/// finer word for it.
///
/// Redirects are reqwest's own business, below the middleware: `traceparent`
/// travels on the copied headers to whatever a redirect points at, which is
/// what a trace wants. No redirect policy is set; these callers post to fixed
/// service paths.
///
/// The name is the method alone because a client call has no route template to
/// aggregate on, and a path with an id in it would scatter one endpoint across
/// many names: a name is a bucket, so the concrete target is an attribute.
pub fn http_client(builder: reqwest::ClientBuilder) -> Result<HttpClient, reqwest::Error> {
    let client = builder.build()?;
    Ok(reqwest_middleware::ClientBuilder::new(client)
        .with(TracePropagation)
        .build())
}

/// Carries one call's context out with the request, and its outcome back onto
/// the span opened for it.
#[derive(Debug, Clone, Copy)]
struct TracePropagation;

#[async_trait::async_trait]
impl Middleware for TracePropagation {
    async fn handle(
        &self,
        mut request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> Result<reqwest::Response, reqwest_middleware::Error> {
        use opentelemetry::trace::Status;
        use tracing::Instrument as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        let url_path = capped(request.url().path(), URL_PATH_LIMIT).to_string();
        let method = request.method().clone();
        // The name stops at the method: a client call has no route template to
        // aggregate on, and a path with an id in it would scatter one endpoint
        // across many names. The target travels in the attributes below.
        let span = tracing::info_span!(
            target: SPAN_TARGET,
            "request",
            otel.name = %method,
            otel.kind = %CLIENT_SPAN_KIND,
        );
        // The parent comes from the layer: it adopts the current span's OTel context.
        span.set_attribute("http.request.method", method.as_str().to_string());
        span.set_attribute("url.path", url_path);
        // Where the call went: the name the URL writes, not the address a
        // resolver would hand back -- a span should say which service was
        // asked, the way the configuration spells it, not which replica
        // answered.
        if let Some(host) = request.url().host_str() {
            span.set_attribute("server.address", host.to_string());
        }
        if let Some(port) = request.url().port_or_known_default() {
            span.set_attribute("server.port", i64::from(port));
        }
        // Reading the context builds the span if it is still a builder, so the
        // headers carry this call's own span id and not the caller's.
        let context = span.context();
        {
            let mut injector = HeaderInjector(request.headers_mut());
            opentelemetry::global::get_text_map_propagator(|propagator| {
                propagator.inject_context(&context, &mut injector);
            });
        }
        let result = next.run(request, extensions).instrument(span.clone()).await;
        match &result {
            Ok(response) => {
                let status = response.status();
                span.set_attribute("http.response.status_code", i64::from(status.as_u16()));
                if status.is_client_error() || status.is_server_error() {
                    span.set_attribute("error.type", format!("http_{}", status.as_u16()));
                    span.set_status(Status::error(status.to_string()));
                }
            }
            Err(error) => {
                let kind = failure_type(error);
                span.set_attribute("error.type", kind);
                span.set_status(Status::error(kind));
            }
        }
        result
    }
}

/// What a failed call names as `error.type` and as its status: the shape of
/// the failure, never its text. A failure inside the middleware chain is
/// named apart from the failures below it, so a middleware bug is not a
/// network fault in the trace.
fn failure_type(error: &reqwest_middleware::Error) -> &'static str {
    match error {
        reqwest_middleware::Error::Middleware(_) => "middleware",
        reqwest_middleware::Error::Reqwest(error) if error.is_timeout() => "timeout",
        reqwest_middleware::Error::Reqwest(_) => "transport",
    }
}

/// The request headers as the propagator writes them. A name or a value the
/// header map cannot hold is dropped: carrying the context is worth a header,
/// never a failed call.
struct HeaderInjector<'a>(&'a mut reqwest::header::HeaderMap);

impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, value);
        }
    }
}
