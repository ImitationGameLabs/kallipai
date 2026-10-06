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

use async_trait::async_trait;
use pingora::Result;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::{HttpPeer, Server};
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};

pub use context::{GatewayCtx, Route};

use crate::distribution::TagmaIdentity;
use crate::forward;
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

    /// Authenticate against the session's headers; on failure the error
    /// response is written (with the CORS headers) and `None` tells the
    /// caller to stop the phase.
    async fn authenticate_or_respond(
        &self,
        session: &mut Session,
    ) -> Result<Option<TagmaIdentity>> {
        match routes::authenticate(&self.state, &session.req_header().headers).await {
            Ok(identity) => Ok(Some(identity)),
            Err(response) => {
                let origin = request_origin(session);
                wire::respond_axum(session, *response, &self.cors, origin.as_deref()).await?;
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
                if self.body_too_large(session) {
                    return wire::respond_axum(
                        session,
                        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                        &self.cors,
                        origin.as_deref(),
                    )
                    .await;
                }
                let Some(identity) = self.authenticate_or_respond(session).await? else {
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
                        Ok(false)
                    }
                    Err(response) => {
                        wire::respond_axum(session, *response, &self.cors, origin.as_deref())
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
}
