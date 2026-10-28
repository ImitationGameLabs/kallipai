//! The pingora data plane: the forwarding and health faces
//! on one listener (7501), written directly against the `ProxyHttp`
//! phases.
//!
//! Phase map:
//! - `request_filter`: the CORS preflight, the admin short-circuit,
//!   tagma-identity authentication, profile selection (the visibility /
//!   parking / family gates) and the upstream-credential resolution.
//!   Everything that can answer 4xx/5xx without an upstream happens
//!   here.
//! - `upstream_peer`: builds the dial target for the selection's next
//!   candidate, and answers with no candidate only once the cursor's
//!   gate has closed.
//! - `upstream_request_filter`: the wire-path rewrite, the credential
//!   header injection and the safe-header passthrough.
//! - `response_filter`: the CORS response headers.
//! - `upstream_response_filter`: t1 and the upstream status line,
//!   staged for the terminal classification; the first-header latency
//!   is observed here.
//! - `connected_to_upstream` / `fail_to_connect`: the connection
//!   instruments (establishment duration from the pingora digest;
//!   failures by class), and the retry decision a failed dial gets.
//! - `logging`: the terminal phase -- the single point every forwarded
//!   request is counted at, and where the in-flight gauge decrements.
//!
//! The phases that answer and account run inside one request span:
//! `request_filter` opens it before anything answers and runs the body that
//! answers under it; `upstream_request_filter`, `response_filter` and
//! `upstream_response_filter` re-enter it for their own body; `logging`
//! records the terminal status and outcome on it. A refusal, a health probe
//! and a forwarded request each land in one span, so a call this process
//! makes while serving one -- the identity check -- is its child instead of
//! a trace of its own. The store work sea-orm emits is traced at trace
//! level, so it hangs under this span too when `RUST_LOG` lets that level
//! through.
//!
//! The connection callbacks -- `upstream_peer`, `connected_to_upstream` and
//! `fail_to_connect` -- stay outside the span: they resolve the peer or
//! record dial instruments, and no span is opened for a dial.
//!
//! The business layer keeps speaking axum types: the routing module
//! (`routes`) runs the 4xx gates as `ApiError`s, and `wire` bridges
//! the rendered responses to the session. Forwarded traffic never
//! passes through the bridge -- the framework proxies it verbatim.

mod context;
#[cfg(test)]
mod failure_line_tests;
mod resolver;
pub mod routes;
#[cfg(test)]
mod tests;
mod wire;

use std::time::Instant;

use async_trait::async_trait;
use axum::response::IntoResponse;
use kallipai_common::telemetry;
use pingora::Result;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::{HttpPeer, Server};
use pingora::protocols::Digest;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};
use tracing::Instrument as _;

use context::Selection;
pub use context::{GatewayCtx, Route};
use resolver::Resolver;

use crate::distribution::TagmaIdentity;
use crate::forward;
use crate::metrics::{ConnectErrorClass, ForwardOutcome, IdentityOutcome, UpstreamStatusClass};
use crate::state::AppState;

/// The application: shared state plus the CORS allow-origin policy.
pub struct GatewayProxy {
    pub state: AppState,
    pub resolver: Resolver,
    pub cors: wire::CorsPolicy,
}

impl GatewayProxy {
    /// The declared request-body bound, refused before any selection
    /// work. A chunked body without a declared length cannot be
    /// pre-checked here and streams through: pingora does not buffer
    /// the request body, so the bound sees a declared content-length
    /// only.
    fn body_too_large(&self, session: &Session) -> bool {
        let declared = session
            .req_header()
            .headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        matches!(declared, Some(len) if len > forward::MAX_BODY_BYTES)
    }

    /// Authenticate against the session's headers. The resolution is
    /// counted where it happens (the identity face is its own metric);
    /// on failure the error response is written (with the CORS
    /// headers), the forward outcome is staged for the terminal
    /// record, and `None` tells the caller to stop the phase.
    async fn authenticate_or_respond(
        &self,
        session: &mut Session,
        ctx: &mut GatewayCtx,
    ) -> Result<Option<TagmaIdentity>> {
        match routes::authenticate(&self.state, &session.req_header().headers).await {
            Ok(identity) => {
                self.state.metrics.record_identity(IdentityOutcome::Ok);
                Ok(Some(identity))
            }
            Err(error) => {
                self.state
                    .metrics
                    .record_identity(IdentityOutcome::from_status(error.status));
                ctx.pending_outcome = Some(ForwardOutcome::GatedAuth);
                let origin = request_origin(session);
                wire::respond_axum(
                    session,
                    error.into_response(),
                    &self.cors,
                    origin.as_deref(),
                )
                .await?;
                Ok(None)
            }
        }
    }
    /// The request proper, run inside the request span: the CORS
    /// preflight, the routing table, and the Forward branch.
    async fn answer_request(
        &self,
        session: &mut Session,
        ctx: &mut GatewayCtx,
        origin: Option<String>,
    ) -> Result<bool> {
        // The CORS preflight: the browser's probe, answered before any routing.
        if wire::is_cors_preflight(session) {
            return wire::respond_axum(
                session,
                axum::http::StatusCode::OK,
                &self.cors,
                origin.as_deref(),
            )
            .await;
        }
        match ctx.route {
            // The management namespace is a 404 on this plane, bare
            // prefix included: the axum face owns it on its own listener,
            // so an /admin hit on the public port is a client mistake.
            Route::Admin | Route::NotFound => {
                wire::respond_axum(
                    session,
                    crate::routes::ApiError::not_found("no such resource"),
                    &self.cors,
                    origin.as_deref(),
                )
                .await
            }
            // A known path with the wrong method is a bare 405.
            Route::MethodNotAllowed => {
                wire::respond_axum(
                    session,
                    axum::http::StatusCode::METHOD_NOT_ALLOWED,
                    &self.cors,
                    origin.as_deref(),
                )
                .await
            }
            Route::Health => wire::respond_axum(session, "ok", &self.cors, origin.as_deref()).await,
            Route::Forward(endpoint) => {
                ctx.t0 = Some(Instant::now());
                if self.body_too_large(session) {
                    ctx.pending_outcome = Some(ForwardOutcome::GatedPayload);
                    return wire::respond_axum(
                        session,
                        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                        &self.cors,
                        origin.as_deref(),
                    )
                    .await;
                }
                let Some(identity) = self.authenticate_or_respond(session, ctx).await? else {
                    return Ok(true);
                };
                // The authenticated tagma is staged for the terminal
                // failure line: the caller behind a failure is named.
                ctx.tagma_id = Some(identity.tagma_id.clone());
                // The selection decides everything the later phases do;
                // an error here is the client-visible answer.
                match routes::select_route(
                    &self.state,
                    &self.resolver,
                    &identity,
                    &session.req_header().headers,
                    endpoint,
                )
                .await
                {
                    Ok(selection) => {
                        ctx.selection = Some(selection);
                        self.state.metrics.inc_in_flight();
                        Ok(false)
                    }
                    Err(gate) => {
                        ctx.pending_outcome = Some(gate.outcome);
                        ctx.pending_reason = gate.reason;
                        ctx.pending_detail = gate.detail.clone();
                        wire::respond_axum(
                            session,
                            gate.into_response(),
                            &self.cors,
                            origin.as_deref(),
                        )
                        .await
                        .map(|_| true)
                    }
                }
            }
        }
    }

    /// The upstream rewrite: the wire path, the header surgery and the
    /// credential stamp.
    async fn rewrite_upstream(
        &self,
        upstream_request: &mut RequestHeader,
        ctx: &mut GatewayCtx,
    ) -> Result<()> {
        // The wire path's route: `request_filter` forwards only
        // `Route::Forward`, so a rewrite on another route is a flow slip.
        // It returns a pingora internal error rather than panicking; this
        // route never enters the forwarding face the `logging` phase
        // records, so no failure line is written for it.
        let Route::Forward(endpoint) = ctx.route else {
            return Err(pingora::Error::explain(
                pingora::ErrorType::InternalError,
                PHASE_SLIP_NOT_FORWARD,
            ));
        };
        // The selection `request_filter` staged, checked the same way.
        // Read only: the rewrite moves no candidate, so the cursor stays put.
        let selection: &Selection = staged_selection(ctx)?;
        // The wire path: the credential's base (trimmed) plus the
        // endpoint's resource path. The client's query string is not
        // forwarded: the generation APIs take everything in the body.
        let path = format!(
            "{}{}",
            selection.base_path.trim_end_matches('/'),
            endpoint.wire_path()
        );
        let uri = axum::http::Uri::builder()
            .path_and_query(path)
            .build()
            .map_err(|e| {
                tracing::error!(error = %e, "rewriting the upstream path failed");
                pingora::Error::new(pingora::ErrorType::Custom("invalid upstream path"))
            })?;
        upstream_request.set_uri(uri);

        // Identity and hop-by-hop headers are rebuilt for the upstream;
        // the caller's authorization and the profile-override header
        // never leave the proxy; the wire credential headers are
        // stripped too -- the upstream's copies are always the gateway's
        // own stamp from the credential, never the client's.
        // `accept-encoding` is stripped as well: a compressed upstream
        // body would otherwise reach the caller with its
        // `content-encoding` dropped -- labeled corruption.
        for name in [
            &axum::http::header::ACCEPT_ENCODING,
            &axum::http::header::AUTHORIZATION,
            &axum::http::header::HOST,
            &axum::http::header::CONTENT_LENGTH,
            &axum::http::header::CONNECTION,
        ] {
            let _ = upstream_request.remove_header(name);
        }
        for name in ["x-kallipai-profile", "x-api-key", "anthropic-version"] {
            let name =
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a static header name");
            let _ = upstream_request.remove_header(&name);
        }

        // The credential stamp: the wire family's credential headers
        // from the credential type's own egress method.
        for (name, value) in selection.credential.auth_headers(selection.auth) {
            upstream_request.insert_header(name, value)?;
        }
        // The Host header is the upstream authority, verbatim from the
        // credential's URL.
        upstream_request.insert_header("host", selection.authority.clone())?;
        Ok(())
    }

    /// The first upstream header: t1, the status line, and the two
    /// observations the terminal phase classifies from.
    async fn observe_upstream_response(
        &self,
        upstream_response: &ResponseHeader,
        ctx: &mut GatewayCtx,
    ) -> Result<()> {
        // The first upstream header: t1 and the upstream status line,
        // both staged for the terminal classification. The first-header
        // latency and the response-class attribution are observed here.
        ctx.t1 = Some(Instant::now());
        ctx.upstream_status = Some(upstream_response.status.as_u16());
        if let (Some(t0), Some(t1), Some(selection)) = (ctx.t0, ctx.t1, ctx.selection.as_ref()) {
            self.state
                .metrics
                .observe_time_to_first_header(&selection.authority, t1.duration_since(t0));
        }
        if let Some(selection) = ctx.selection.as_ref() {
            self.state.metrics.record_upstream_response(
                &selection.authority,
                &selection.api_family,
                UpstreamStatusClass::from_status(upstream_response.status.as_u16()),
            );
        }
        Ok(())
    }

    /// The terminal record: the span's status for every route, and the
    /// forwarded traffic's outcome, failure line and instruments.
    async fn finish_request(
        &self,
        session: &mut Session,
        e: Option<&pingora::Error>,
        ctx: &mut GatewayCtx,
    ) {
        let span = ctx.phase_span();
        // The status every answer carried, forwarded or not: a refusal
        // and a health probe have a response too, and a trace needs
        // their outcome as much as a forwarded request's.
        if let Some(response) = session.response_written() {
            telemetry::record_response(&span, response.status.as_u16());
        }
        // The single terminal record point: every forwarded request is
        // counted here exactly once, from whatever the phases before
        // staged (the derivation lives in [`terminal_outcome`]).
        let Route::Forward(endpoint) = ctx.route else {
            return;
        };
        let outcome = terminal_outcome(
            ctx.pending_outcome.take(),
            e,
            ctx.connected,
            ctx.upstream_status,
        );
        telemetry::record_attribute(&span, "gateway.outcome", outcome.as_str());
        self.state.metrics.record_forward(endpoint, outcome);
        // The failure line: one structured record per failed forward,
        // carrying the attribution the counters carry.
        log_forward_failure(
            endpoint,
            outcome,
            ctx,
            e,
            ctx.t0.map(|t0| Instant::now().duration_since(t0)),
        );
        // The attribution the terminal record carries -- the upstream
        // label the metrics carry too -- and the instruments: the
        // in-flight gauge pairs with its selection-time increment, and
        // the end-to-end duration is a forwarded-traffic histogram
        // (instant gate rejections would drown the long tail it exists
        // to show).
        if let Some(selection) = ctx.selection.as_ref() {
            telemetry::record_attribute(&span, "upstream.authority", selection.authority.clone());
            telemetry::record_attribute(&span, "upstream.api_family", selection.api_family.clone());
            self.state.metrics.dec_in_flight();
            if let Some(t0) = ctx.t0 {
                self.state
                    .metrics
                    .observe_forward_duration(endpoint, Instant::now().duration_since(t0));
            }
        }
    }

    /// The CORS headers the caller's origin is allowed: the same allowlist
    /// policy the bridge answers use.
    async fn apply_cors_headers(
        &self,
        session: &Session,
        upstream_response: &mut ResponseHeader,
    ) -> Result<()> {
        for (name, value) in self.cors.headers(request_origin(session).as_deref()) {
            upstream_response.insert_header(name, value)?;
        }
        Ok(())
    }
}

/// The request's `Origin` header, the CORS policy's discriminator.
fn request_origin(session: &Session) -> Option<String> {
    session
        .req_header()
        .headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Spawn the data plane on its own thread (pingora drives its own
/// runtime there) so the caller's tokio runtime keeps serving the axum
/// management face in the same process.
///
/// The socket is probe-bound on the caller's thread first: pingora
/// binds inside its own `run_forever`, on the spawned thread, where a
/// failure would surface as nothing louder than a background panic
/// while the caller's log already claims the port. A taken port or a
/// bad address therefore returns as the caller's error. The probe
/// drops before pingora binds for real; the window between the two
/// binds is the accepted race.
pub fn spawn_data_plane(
    listen: String,
    state: AppState,
    cors_origins: String,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let resolver = Resolver::system(state.metrics.clone());
    spawn_data_plane_with_resolver(listen, state, cors_origins, resolver)
}

/// `spawn_data_plane` with the resolver handed in, so a caller can pin
/// the addresses a dial walks: the tests drive failover this way, while
/// the production path passes the system resolver.
pub(crate) fn spawn_data_plane_with_resolver(
    listen: String,
    state: AppState,
    cors_origins: String,
    resolver: Resolver,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    drop(std::net::TcpListener::bind(&listen)?);
    std::thread::Builder::new()
        .name("data-plane-pingora".to_string())
        .spawn(move || {
            let mut server = Server::new(None).expect("pingora server");
            server.bootstrap();
            let mut service = http_proxy_service(
                &server.configuration,
                GatewayProxy {
                    state,
                    resolver,
                    cors: wire::CorsPolicy::parse(&cors_origins),
                },
            );
            service.add_tcp(&listen);
            server.add_service(service);
            server.run_forever();
        })
}

#[async_trait]
impl ProxyHttp for GatewayProxy {
    type CTX = GatewayCtx;

    fn new_ctx(&self) -> Self::CTX {
        GatewayCtx {
            route: Route::NotFound,
            span: None,
            selection: None,
            tagma_id: None,
            t0: None,
            t1: None,
            pending_outcome: None,
            pending_reason: None,
            pending_detail: None,
            upstream_status: None,
            connected: false,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let method = session.req_header().method.clone();
        let path = session.req_header().uri.path().to_owned();
        // The route is a pure function of the method and the path, so it
        // is decided before anything answers and stored on the context: the
        // span names it, except a preflight, which names itself; every later
        // phase reads the same value back.
        let route = routes::route_of(&method, &path);
        ctx.route = route;
        // A preflight is an OPTIONS probe at a path whose method is POST,
        // so it is no routed path: it labels itself instead of borrowing
        // the 405 its method would key.
        let label = if wire::is_cors_preflight(session) {
            "preflight"
        } else {
            route.label()
        };
        let span =
            telemetry::request_span(method.as_str(), label, &path, &session.req_header().headers);
        ctx.span = Some(span.clone());
        self.answer_request(session, ctx, request_origin(session))
            .instrument(span)
            .await
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Every dial starts from the same instrument state: a second
        // ask after a failed dial must not read the first one's facts.
        ctx.t1 = None;
        ctx.upstream_status = None;
        ctx.connected = false;
        // The cursor hands out the next candidate while one remains and
        // the dial budget is unspent. An empty answer is pingora asking
        // for a dial this request has no address for: the list or the
        // budget ended the walk, and no peer is invented to cover it.
        let selection = staged_selection(ctx)?;
        let Some(peer) = selection.next_peer() else {
            // This error carries no cause, so `Error::create` hands the
            // retry loop a `Decided(false)` and it stops here at once: the
            // walk is over, and asking the same cursor again repeats the
            // question. A cause added here later would lend its own
            // `RetryType`, so the site would have to answer for the retry
            // itself rather than inherit this.
            return Err(pingora::Error::new_up(pingora::ErrorType::ConnectError)
                .more_context(NO_CANDIDATE_LEFT));
        };
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let span = ctx.phase_span();
        self.rewrite_upstream(upstream_request, ctx)
            .instrument(span)
            .await
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let span = ctx.phase_span();
        self.apply_cors_headers(session, upstream_response)
            .instrument(span)
            .await
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let span = ctx.phase_span();
        self.observe_upstream_response(upstream_response, ctx)
            .instrument(span)
            .await
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.connected = true;
        // The digest's establishment durations belong to this
        // connection: a reused one was established for an earlier
        // request, and observing it again would count one dial twice.
        // Layers the producer did not measure are skipped.
        if reused {
            return Ok(());
        }
        let Some(digest) = digest else {
            return Ok(());
        };
        let total = digest
            .timing_digest
            .iter()
            .filter_map(|layer| layer.as_ref().and_then(|t| t.establishment_duration))
            .sum::<std::time::Duration>();
        if let Some(selection) = ctx.selection.as_ref() {
            self.state
                .metrics
                .observe_connect_duration(&selection.authority, total);
        }
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut e: Box<pingora::Error>,
    ) -> Box<pingora::Error> {
        // One failed dial, not one failed request: a retry redials and
        // counts here again, while the terminal phase still counts the
        // request once.
        if let Some(selection) = ctx.selection.as_ref() {
            self.state
                .metrics
                .record_connect_failure(&selection.authority, connect_error_class(e.etype()));
        }
        // The gate is read before the next dial is asked for: an address
        // is worth another dial only while one remains and the budget
        // lasts, and this answer is what turns that into a retry.
        let can_retry = ctx.selection.as_ref().is_some_and(|s| s.can_retry());
        // Both arms carry weight: the gate says whether a candidate is
        // left, the type filter whether a different address can mend the
        // failure. `set_retry` always writes `Decided`, so the reused-
        // connection arm that panics `retry()` stays unreachable.
        e.set_retry(retry_connect_error(e.etype(), can_retry));
        e
    }

    async fn logging(
        &self,
        session: &mut Session,
        e: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
    ) {
        let span = ctx.phase_span();
        self.finish_request(session, e, ctx).instrument(span).await
    }
}

/// The terminal forward outcome, decided once at the logging phase
/// from the staged facts: a gate's pending outcome outranks every
/// derived fact, a failure the downstream side raised outranks the
/// contact facts, an error after upstream contact is an upstream error
/// while one without contact never got past the dial, a clean answer
/// derives from the upstream status, and the residual bucket keeps the
/// outcome sums closed.
fn terminal_outcome(
    pending: Option<ForwardOutcome>,
    error: Option<&pingora::Error>,
    connected: bool,
    upstream_status: Option<u16>,
) -> ForwardOutcome {
    if let Some(pending) = pending {
        pending
    } else if let Some(error) = error {
        if ended_downstream(error) {
            ForwardOutcome::DownstreamError
        } else if connected || upstream_status.is_some() {
            ForwardOutcome::UpstreamError
        } else {
            ForwardOutcome::ConnectError
        }
    } else if let Some(status) = upstream_status {
        ForwardOutcome::from_upstream_status(status)
    } else {
        // Selected without an error or an upstream answer: nothing
        // in the current phase flow produces this; the residual
        // bucket keeps the outcome sums closed.
        ForwardOutcome::UpstreamOther
    }
}
/// Whether the error that ended the forward was raised by the downstream
/// side: pingora marks a client request-read or response-write failure --
/// and a request its own sanitize step refused -- with
/// [`pingora::ErrorSource::Downstream`], the same field its `fail_to_proxy`
/// splits on. An unmarked error is not claimed as the caller's failure.
fn ended_downstream(error: &pingora::Error) -> bool {
    *error.esource() == pingora::ErrorSource::Downstream
}

/// One failure line at the severity the outcome deserves: the field set
/// is identical at both severities, so the level picks the macro and the
/// fields are written once.
macro_rules! failure_line {
    ($level:expr, $($field:tt)*) => {
        match $level {
            tracing::Level::ERROR => tracing::error!($($field)*),
            _ => tracing::warn!($($field)*),
        }
    };
}

/// The two phase-flow slips a forwarding phase can meet, each naming
/// the flaw it found. They are answered two different ways. A phase
/// that runs without the selection `request_filter` stages is a
/// failure the gateway records: the slip is staged and the `logging`
/// phase writes it as an `internal_error` line with no reason
/// category, since no input caused it. A rewrite that runs on a route
/// which does not forward has no line at all: that route never enters
/// the forwarding face the `logging` phase records, so the slip is
/// only the text on the pingora error it returns. Neither panics the
/// worker.
const PHASE_SLIP_NO_SELECTION: &str = "a forwarding phase ran without a selection";
const PHASE_SLIP_NOT_FORWARD: &str = "the rewrite phase ran on a route that does not forward";

/// The text an exhausted candidate cursor answers with. No dial was made,
/// so it must not read like an upstream refusing a connection: the
/// operator tells the two apart by this line, and `connect_error_class`
/// never sees it.
const NO_CANDIDATE_LEFT: &str =
    "no candidate left to dial: the resolver's list is exhausted or the dial budget is spent";

/// The selection a forwarding phase runs on, staged by
/// `request_filter` before any later phase runs. It is handed out
/// mutably: the peer phase advances the candidate cursor on it,
/// and the other phases only read. A phase that finds none is a
/// flow slip: it is recorded as the gateway's own failure (no
/// reason category -- no input caused it) and also answered by
/// pingora's error path, so the worker is not panicked.
fn staged_selection(ctx: &mut GatewayCtx) -> Result<&mut Selection, Box<pingora::Error>> {
    let selection = match ctx.selection.as_mut() {
        Some(selection) => selection,
        None => {
            ctx.pending_outcome = Some(ForwardOutcome::InternalError);
            ctx.pending_detail = Some(PHASE_SLIP_NO_SELECTION.to_string());
            return Err(pingora::Error::explain(
                pingora::ErrorType::InternalError,
                PHASE_SLIP_NO_SELECTION,
            ));
        }
    };
    Ok(selection)
}

/// The severity a failed forward records at: the gateway's own
/// forwarding trouble -- a dial failure, a failed upstream resolution, an
/// upstream-side mid-stream break, or a gateway-side internal failure -- is
/// an ERROR, while a gate rejection, the upstream's own terminations, and a
/// downstream side's failure are a WARN (the request was refused, answered, or
/// ended on the caller's side, not the platform's health dented). Every
/// variant is spelled out: a new outcome cannot land on a severity by
/// falling into a catch-all.
fn failure_level(outcome: ForwardOutcome) -> tracing::Level {
    match outcome {
        ForwardOutcome::ConnectError
        | ForwardOutcome::UpstreamError
        | ForwardOutcome::InternalError
        | ForwardOutcome::ResolveError => tracing::Level::ERROR,
        ForwardOutcome::GatedAuth
        | ForwardOutcome::GatedRoute
        | ForwardOutcome::GatedVisibility
        | ForwardOutcome::GatedParking
        | ForwardOutcome::GatedPayload
        | ForwardOutcome::Upstream2xx
        | ForwardOutcome::Upstream4xx
        | ForwardOutcome::Upstream5xx
        | ForwardOutcome::UpstreamOther => tracing::Level::WARN,
        ForwardOutcome::DownstreamError => tracing::Level::WARN,
    }
}

/// Whether the terminal phase writes the failure line: an error
/// terminates the forward on the wire, and a gate refused the client
/// before one was reached. The gates -- and the two failures the
/// gateway answers itself, which render their own 5xx the same way --
/// answer the client themselves, so the logging phase sees no error for
/// them: their line warrants on the outcome alone. The upstream answers
/// (2xx through other) are the request's normal terminations and
/// stay off the failure stream.
fn warrants_failure_line(outcome: ForwardOutcome, errored: bool) -> bool {
    errored || outcome.is_gate() || outcome.answers_itself()
}

/// The failure line: one structured record per failed forward, naming
/// the endpoint and the outcome the counters carry, so a log reader
/// and a dashboard reader see the same request. A field only a
/// selection holds -- `authority` and `profile_id` -- is absent from
/// the line on a failure that came before one existed (the identity
/// gate, a failed resolution). The counter face differs there: a
/// resolution failure knows the authority before it dials, so its
/// counter labels the host under `upstream`, while the forward outcome
/// counter carries the endpoint and outcome alone. The host is not
/// missing from the line either: it sits
/// in the `error` text, `resolving the upstream <authority> for profile
/// <id> failed: ...`. It warrants on an error (the wire failure path)
/// or on a gate outcome; the gate line carries no error field, while a
/// failure the gateway answers itself names the error that ended it and
/// the reason category behind it. The clock is absent before the
/// forward branch staged it.
fn log_forward_failure(
    endpoint: forward::dialect::Endpoint,
    outcome: ForwardOutcome,
    ctx: &GatewayCtx,
    error: Option<&pingora::Error>,
    elapsed: Option<std::time::Duration>,
) {
    if !warrants_failure_line(outcome, error.is_some()) {
        return;
    }
    let selection = ctx.selection.as_ref();
    // The line's `error` field: the wire error when one ended the request,
    // else the gateway-side failure's own message -- the response body
    // carries the redacted `internal error` text instead. Both can be in
    // hand on one request (an internal failure after a wire error); the
    // wire error is the one that ended the forward, so it wins.
    let wire_error = error.map(|error| error.to_string());
    failure_line!(
        failure_level(outcome),
        endpoint = crate::metrics::endpoint_label(endpoint),
        outcome = outcome.as_str(),
        api_family = selection.map(|s| s.api_family.as_str()),
        authority = selection.map(|s| s.authority.as_str()),
        profile_id = selection.map(|s| s.profile_id.as_str()),
        tagma_id = ctx.tagma_id.as_deref(),
        upstream_status = ctx.upstream_status,
        connected = ctx.connected,
        elapsed_ms = elapsed.map(|d| d.as_millis() as u64),
        reason = ctx.pending_reason.map(|reason| reason.as_str()),
        error = wire_error.or_else(|| ctx.pending_detail.clone()),
        "the forward failed"
    );
}

/// The pingora error taxonomy folded into the four connect-failure
/// classes the observability face counts.
fn connect_error_class(error: &pingora::ErrorType) -> ConnectErrorClass {
    use pingora::ErrorType::{
        ConnectRefused, ConnectTimedout, HandshakeError, InvalidCert, TLSHandshakeFailure,
        TLSHandshakeTimedout, TLSWantX509Lookup,
    };
    match error {
        ConnectTimedout => ConnectErrorClass::Timeout,
        ConnectRefused => ConnectErrorClass::Refused,
        TLSWantX509Lookup | TLSHandshakeFailure | TLSHandshakeTimedout | InvalidCert
        | HandshakeError => ConnectErrorClass::Tls,
        _ => ConnectErrorClass::Other,
    }
}

/// Whether a failed dial earns another candidate. `can_retry` is the
/// cursor's gate, and the type filter keeps the answer off failures a
/// second address cannot mend: a certificate the client rejects or a
/// socket it cannot build fails the same way wherever it dials, while
/// the arms listed are the connect and TLS-handshake failures a
/// different address can survive.
fn retry_connect_error(error: &pingora::ErrorType, can_retry: bool) -> bool {
    use pingora::ErrorType::{
        ConnectError, ConnectNoRoute, ConnectProxyFailure, ConnectRefused, ConnectTimedout,
        HandshakeError, TLSHandshakeFailure, TLSHandshakeTimedout,
    };
    can_retry
        && matches!(
            error,
            ConnectRefused
                | ConnectTimedout
                | ConnectNoRoute
                | ConnectError
                | ConnectProxyFailure
                | TLSHandshakeFailure
                | TLSHandshakeTimedout
                | HandshakeError
        )
}

#[cfg(test)]
mod connect_class_tests {
    use super::connect_error_class;
    use crate::metrics::ConnectErrorClass;
    use pingora::ErrorType;

    #[test]
    fn classes() {
        assert_eq!(
            connect_error_class(&ErrorType::ConnectTimedout),
            ConnectErrorClass::Timeout
        );
        assert_eq!(
            connect_error_class(&ErrorType::ConnectRefused),
            ConnectErrorClass::Refused
        );
        assert_eq!(
            connect_error_class(&ErrorType::TLSHandshakeFailure),
            ConnectErrorClass::Tls
        );
        assert_eq!(
            connect_error_class(&ErrorType::InvalidCert),
            ConnectErrorClass::Tls
        );
        // No-route and the catch-all connect error have no finer
        // bucket.
        assert_eq!(
            connect_error_class(&ErrorType::ConnectNoRoute),
            ConnectErrorClass::Other
        );
        assert_eq!(
            connect_error_class(&ErrorType::ConnectError),
            ConnectErrorClass::Other
        );
    }
}

#[cfg(test)]
mod retry_connect_error_tests {
    use super::retry_connect_error;
    use pingora::ErrorType;

    #[test]
    fn a_dial_failure_earns_another_candidate_while_the_gate_is_open() {
        for error in [
            ErrorType::ConnectRefused,
            ErrorType::ConnectTimedout,
            ErrorType::ConnectNoRoute,
            ErrorType::ConnectError,
            ErrorType::ConnectProxyFailure,
            ErrorType::TLSHandshakeFailure,
            ErrorType::TLSHandshakeTimedout,
            ErrorType::HandshakeError,
        ] {
            assert!(retry_connect_error(&error, true), "{error:?}");
        }
    }

    #[test]
    fn a_failure_no_other_address_can_mend_is_not_retried() {
        for error in [
            ErrorType::InvalidCert,
            ErrorType::TLSWantX509Lookup,
            ErrorType::InternalError,
            ErrorType::BindError,
            ErrorType::SocketError,
        ] {
            assert!(!retry_connect_error(&error, true), "{error:?}");
        }
    }

    #[test]
    fn a_closed_gate_overrides_every_class() {
        for error in [
            ErrorType::ConnectRefused,
            ErrorType::ConnectTimedout,
            ErrorType::TLSHandshakeFailure,
        ] {
            assert!(!retry_connect_error(&error, false), "{error:?}");
        }
    }
}

/// The terminal-outcome table, pinned cell by cell: the pending gate
/// answer outranks the facts, a downstream failure outranks the contact
/// facts, the error arm splits on contact, the clean arm derives from
/// the status, and the residual stays closed.
#[cfg(test)]
mod terminal_outcome_tests {
    use super::{ended_downstream, terminal_outcome};
    use crate::metrics::ForwardOutcome;
    use pingora::ErrorType;

    fn up_error() -> Box<pingora::Error> {
        pingora::Error::new_up(ErrorType::ConnectionClosed)
    }

    fn down_error() -> Box<pingora::Error> {
        pingora::Error::new_down(ErrorType::ConnectionClosed)
    }

    #[test]
    fn a_pending_gate_answer_outranks_every_derived_fact() {
        for (error, connected, status) in [
            (None, false, None),
            (Some(up_error()), false, None),
            (Some(up_error()), true, None),
            (Some(up_error()), true, Some(502)),
            (Some(down_error()), true, Some(200)),
            (None, true, Some(200)),
        ] {
            assert_eq!(
                terminal_outcome(
                    Some(ForwardOutcome::GatedVisibility),
                    error.as_deref(),
                    connected,
                    status
                ),
                ForwardOutcome::GatedVisibility
            );
        }
    }

    #[test]
    fn an_error_splits_on_upstream_contact() {
        assert_eq!(
            terminal_outcome(None, Some(&up_error()), false, None),
            ForwardOutcome::ConnectError
        );
        for (connected, status) in [(true, None), (true, Some(500)), (false, Some(500))] {
            assert_eq!(
                terminal_outcome(None, Some(&up_error()), connected, status),
                ForwardOutcome::UpstreamError,
                "connected={connected}, status={status:?}"
            );
        }
    }

    #[test]
    fn a_clean_answer_derives_from_the_upstream_status() {
        assert_eq!(
            terminal_outcome(None, None, true, Some(200)),
            ForwardOutcome::Upstream2xx
        );
        assert_eq!(
            terminal_outcome(None, None, true, Some(404)),
            ForwardOutcome::Upstream4xx
        );
        assert_eq!(
            terminal_outcome(None, None, true, Some(503)),
            ForwardOutcome::Upstream5xx
        );
    }

    #[test]
    fn the_residual_bucket_stays_closed() {
        assert_eq!(
            terminal_outcome(None, None, false, None),
            ForwardOutcome::UpstreamOther
        );
    }

    #[test]
    fn a_downstream_failure_is_the_callers_not_the_upstreams() {
        // A downstream failure outranks the contact facts: it ended the
        // exchange, whether or not an upstream answered first.
        for (connected, status) in [(true, None), (true, Some(200)), (false, None)] {
            assert_eq!(
                terminal_outcome(None, Some(&down_error()), connected, status),
                ForwardOutcome::DownstreamError,
                "connected={connected}, status={status:?}"
            );
        }
        // A gate still outranks it: the gate never reached the wire.
        assert_eq!(
            terminal_outcome(
                Some(ForwardOutcome::GatedRoute),
                Some(&down_error()),
                true,
                Some(200)
            ),
            ForwardOutcome::GatedRoute
        );
    }

    #[test]
    fn the_downstream_mark_comes_from_the_error_itself() {
        assert!(ended_downstream(&down_error()));
        assert!(!ended_downstream(&up_error()));
        let unmarked = pingora::Error::new(ErrorType::WriteError);
        assert!(!ended_downstream(&unmarked));
    }
}
