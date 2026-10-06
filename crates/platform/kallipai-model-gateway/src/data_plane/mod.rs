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
//! - `upstream_peer`: reads the peer the selection left in the context.
//! - `upstream_request_filter`: the wire-path rewrite, the credential
//!   header injection and the safe-header passthrough.
//! - `response_filter`: the CORS response headers.
//! - `upstream_response_filter`: t1 and the upstream status line,
//!   staged for the terminal classification; the first-header latency
//!   is observed here.
//! - `connected_to_upstream` / `fail_to_connect`: the connection
//!   instruments (establishment duration from the pingora digest;
//!   failures by class).
//! - `logging`: the terminal phase -- the single point every forwarded
//!   request is counted at, and where the in-flight gauge decrements.
//!
//! The business layer keeps speaking axum types: the routing module
//! (`routes`) runs the 4xx gates as `ApiError`s, and `wire` bridges
//! the rendered responses to the session. Forwarded traffic never
//! passes through the bridge -- the framework proxies it verbatim.

mod context;
pub mod routes;
#[cfg(test)]
mod tests;
mod wire;

use std::time::Instant;

use async_trait::async_trait;
use axum::response::IntoResponse;
use pingora::Result;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::{HttpPeer, Server};
use pingora::protocols::Digest;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};

pub use context::{GatewayCtx, Route};

use crate::distribution::TagmaIdentity;
use crate::forward;
use crate::metrics::{ConnectErrorClass, ForwardOutcome, IdentityOutcome, UpstreamStatusClass};
use crate::state::AppState;

/// The application: shared state plus the CORS allow-origin policy.
pub struct GatewayProxy {
    pub state: AppState,
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
            selection: None,
            t0: None,
            t1: None,
            pending_outcome: None,
            upstream_status: None,
            connected: false,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let method = session.req_header().method.clone();
        let path = session.req_header().uri.path().to_owned();
        let origin = request_origin(session);

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

        ctx.route = routes::route_of(&method, &path);
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
                // The selection decides everything the later phases do;
                // an error here is the client-visible answer.
                match routes::select_route(
                    &self.state,
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

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // The selection already resolved the dial target; there is
        // nothing left to look up.
        let selection = ctx
            .selection
            .as_ref()
            .expect("a forwarded request always has a selection");
        Ok(Box::new(selection.peer.clone()))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let selection = ctx
            .selection
            .as_ref()
            .expect("a forwarded request always has a selection");
        // The wire path: the credential's base (trimmed) plus the
        // endpoint's resource path. The client's query string is not
        // forwarded: the generation APIs take everything in the body.
        let endpoint = match ctx.route {
            Route::Forward(endpoint) => endpoint,
            _ => unreachable!("the rewrite phase only runs on forwarded requests"),
        };
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

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // The CORS response headers for the caller's origin: the same
        // allowlist policy the bridge answers use.
        for (name, value) in self.cors.headers(request_origin(session).as_deref()) {
            upstream_response.insert_header(name, value)?;
        }
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
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
        e: Box<pingora::Error>,
    ) -> Box<pingora::Error> {
        // One failed dial, not one failed request: a retry redials and
        // counts here again, while the terminal phase still counts the
        // request once.
        if let Some(selection) = ctx.selection.as_ref() {
            self.state
                .metrics
                .record_connect_failure(&selection.authority, connect_error_class(e.etype()));
        }
        e
    }

    async fn logging(
        &self,
        _session: &mut Session,
        e: Option<&pingora::Error>,
        ctx: &mut Self::CTX,
    ) {
        // The single terminal record point: every forwarded request is
        // counted here exactly once, from whatever the phases before
        // staged (the derivation lives in [`terminal_outcome`]).
        let Route::Forward(endpoint) = ctx.route else {
            return;
        };
        let outcome = terminal_outcome(
            ctx.pending_outcome.take(),
            e.is_some(),
            ctx.connected,
            ctx.upstream_status,
        );
        self.state.metrics.record_forward(endpoint, outcome);
        // The forwarded-traffic instruments: the in-flight gauge pairs
        // with its selection-time increment, and the end-to-end
        // duration is a forwarded-traffic histogram (instant gate
        // rejections would drown the long tail it exists to show).
        if ctx.selection.is_some() {
            self.state.metrics.dec_in_flight();
            if let Some(t0) = ctx.t0 {
                self.state
                    .metrics
                    .observe_forward_duration(endpoint, Instant::now().duration_since(t0));
            }
        }
    }
}

/// The terminal forward outcome, decided once at the logging phase
/// from the staged facts: a gate's pending outcome outranks every
/// derived fact, an error after upstream contact is a proxy error
/// while one without contact never got past the dial, a clean answer
/// derives from the upstream status, and the residual bucket keeps
/// the outcome sums closed.
fn terminal_outcome(
    pending: Option<ForwardOutcome>,
    errored: bool,
    connected: bool,
    upstream_status: Option<u16>,
) -> ForwardOutcome {
    if let Some(pending) = pending {
        pending
    } else if errored {
        if connected || upstream_status.is_some() {
            ForwardOutcome::ProxyError
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

/// The terminal-outcome table, pinned cell by cell: the pending gate
/// answer outranks the facts, the error arm splits on contact, the
/// clean arm derives from the status, and the residual stays closed.
#[cfg(test)]
mod terminal_outcome_tests {
    use super::terminal_outcome;
    use crate::metrics::ForwardOutcome;

    #[test]
    fn a_pending_gate_answer_outranks_every_derived_fact() {
        for (errored, connected, status) in [
            (false, false, None),
            (true, false, None),
            (true, true, None),
            (true, true, Some(502)),
            (false, true, Some(200)),
        ] {
            assert_eq!(
                terminal_outcome(
                    Some(ForwardOutcome::GatedVisibility),
                    errored,
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
            terminal_outcome(None, true, false, None),
            ForwardOutcome::ConnectError
        );
        for (connected, status) in [(true, None), (true, Some(500)), (false, Some(500))] {
            assert_eq!(
                terminal_outcome(None, true, connected, status),
                ForwardOutcome::ProxyError,
                "connected={connected}, status={status:?}"
            );
        }
    }

    #[test]
    fn a_clean_answer_derives_from_the_upstream_status() {
        assert_eq!(
            terminal_outcome(None, false, true, Some(200)),
            ForwardOutcome::Upstream2xx
        );
        assert_eq!(
            terminal_outcome(None, false, true, Some(404)),
            ForwardOutcome::Upstream4xx
        );
        assert_eq!(
            terminal_outcome(None, false, true, Some(503)),
            ForwardOutcome::Upstream5xx
        );
    }

    #[test]
    fn the_residual_bucket_stays_closed() {
        assert_eq!(
            terminal_outcome(None, false, false, None),
            ForwardOutcome::UpstreamOther
        );
    }
}
