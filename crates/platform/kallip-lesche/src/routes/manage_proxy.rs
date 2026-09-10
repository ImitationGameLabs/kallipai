//! Manage-plane reverse proxy: `/v1/tagmata/{id}/manage/{*path}`.
//!
//! Bridges the plaintext manage surface: an
//! authenticated operator session is checked against the tunnel's owner
//! (the only application-layer authorization), then the
//! request is fanned down the tagma's tunnel as a
//! [`TunnelInbound::ManageRest`] frame and the plaintext reply POST is
//! awaited. Tunnel offline degrades to an immediate 502; a dropped reply
//! degrades to a 504 after the wait window.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::any;
use kallip_archeion_common::ids::{ParticipantId, TagmaId};
use kallip_archeion_common::principal::Principal;
use kallip_lesche_common::tunnel::{ManageRestReply, TunnelInbound};
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{Mutex, oneshot};

use crate::auth::AuthPrincipal;
use crate::state::SharedConvState;

/// How long the proxy waits for the tagma's plaintext reply before giving up
/// (504).
const REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// In-flight (tagma, req_id) -> reply channel. A request is registered before
type PendingMap = HashMap<(TagmaId, u64), oneshot::Sender<ManageRestReply>>;

/// (tagma, req_id) -> the reply oneshot, registered by the proxy before
/// fanning the frame and resolved by the manage-reply POST.
fn pending() -> &'static Mutex<PendingMap> {
    static PENDING: OnceLock<Mutex<PendingMap>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a pending proxy request from the tagma's manage-reply POST.
/// Returns false when no waiter matches (unknown req_id or late duplicate).
pub async fn resolve(tagma_id: &TagmaId, req_id: u64, reply: ManageRestReply) -> bool {
    let mut pending = pending().lock().await;
    pending
        .remove(&(tagma_id.clone(), req_id))
        .map(|tx| tx.send(reply).is_ok())
        .unwrap_or(false)
}

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tagmata/{id}/manage/{*path}", any(proxy_manage))
}

async fn proxy_manage(
    State(state): State<SharedConvState>,
    // Path runs BEFORE auth on purpose: a route/extractor shape mismatch
    // is a server bug and must surface as its own 500, not masquerade as
    // a 401. The route is `/tagmata/{id}/manage/{*path}`: TWO capture
    // sets (the named id and the wildcard). A single-element `Path` here
    // rejects every request with 500 WrongNumberOfParameters -- the
    // shape must stay a tuple matched to the route's capture count.
    Path((id, path)): Path<(String, String)>,
    AuthPrincipal(principal): AuthPrincipal,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: Option<axum::Json<serde_json::Value>>,
) -> axum::response::Response {
    let user = match principal {
        Principal::User(user_id) => user_id,
        // Tagma-bearer callers have no business on the operator proxy.
        _ => return (StatusCode::FORBIDDEN, "operator session required").into_response(),
    };
    let tagma_id = TagmaId::from(id.clone());

    // Tenant authorization. On the plaintext frame this boundary
    // must be explicit.
    let tx = {
        let registry = state.registry.read().expect("registry lock");
        match registry.presence.get(&ParticipantId::for_tagma(&tagma_id)) {
            Some(entry) if entry.owner == user => entry.tx.clone(),
            Some(_) => {
                return (StatusCode::FORBIDDEN, "not your tagma").into_response();
            }
            None => {
                return (StatusCode::NOT_FOUND, "tagma not online").into_response();
            }
        }
    };

    // Contract (root-approved): the frame path stays query-free. The one
    // sanctioned client filter is `?include=` on /agents -- the proxy parses
    // it here and forwards the keys inside the frame body instead; any
    // other query key is still a 404.
    let mut body = body
        .map(|axum::Json(v)| v)
        .unwrap_or(serde_json::Value::Null);
    if let Some(q) = uri.query() {
        // hand-rolled: only include=key,key lists ride this path, and the
        // lesche has no form-encoding dependency.
        let malformed = q.split('&').any(|pair| pair.split_once('=').is_none());
        let pairs: Vec<(String, String)> = q
            .split('&')
            .filter(|pair| pair.split_once('=').is_some())
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let unknown = malformed || pairs.iter().any(|(k, _)| k != "include");
        let only_agents = path == "agents";
        if unknown || !only_agents {
            return (StatusCode::NOT_FOUND, "query not allowed on the frame").into_response();
        }
        let include: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| k == "include")
            .flat_map(|(_, v)| v.split(',').map(str::trim).map(str::to_owned))
            .filter(|s| !s.is_empty())
            .collect();
        body = serde_json::json!({ "include": include });
    }
    // The wildcard capture arrives WITHOUT the leading slash; the frame
    // path must be absolute so it matches the tagma-side router's modes.
    let frame_path = format!("/{path}");
    let req_id = next_req_id();

    let (reply_tx, reply_rx) = oneshot::channel();
    pending()
        .lock()
        .await
        .insert((tagma_id.clone(), req_id), reply_tx);

    let frame = TunnelInbound::ManageRest {
        req_id,
        method: method.as_str().to_ascii_uppercase(),
        path: frame_path,
        // Fresh UUID per proxied request: collision-free across reconnects.
        trace: kallip_archeion_common::ids::TraceId::random(),
        body,
    };
    if tx.send(frame).is_err() {
        pending().lock().await.remove(&(tagma_id, req_id));
        return (StatusCode::BAD_GATEWAY, "tagma tunnel offline").into_response();
    }

    match tokio::time::timeout(REPLY_WAIT, reply_rx).await {
        Ok(Ok(reply)) => (
            StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            axum::Json(reply.body),
        )
            .into_response(),
        _ => {
            // Slow-leak guard: a timed-out or dropped waiter must not leave
            // its entry in the pending map (req_id never repeats).
            pending().lock().await.remove(&(tagma_id, req_id));
            (StatusCode::GATEWAY_TIMEOUT, "manage reply not received").into_response()
        }
    }
}

static REQ_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_req_id() -> u64 {
    REQ_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialization round-trip: the reply must survive the wire both ways
    /// (lesche -> tagma frame -> tagma -> plaintext reply POST).
    #[test]
    fn manage_rest_frame_and_reply_round_trip() {
        let frame = TunnelInbound::ManageRest {
            req_id: 7,
            method: "GET".to_owned(),
            path: "/agents".to_owned(),
            trace: kallip_archeion_common::ids::TraceId::random(),
            body: serde_json::Value::Null,
        };
        let encoded = serde_json::to_string(&frame).expect("frame encodes");
        assert!(encoded.contains("manage_rest"));

        let reply = ManageRestReply {
            req_id: 7,
            status: 200,
            body: serde_json::json!({"ok": true}),
        };
        let encoded = serde_json::to_string(&reply).expect("reply encodes");
        let decoded: ManageRestReply = serde_json::from_str(&encoded).expect("reply decodes");
        assert_eq!(decoded.req_id, 7);
        assert_eq!(decoded.status, 200);
    }

    /// Pending bookkeeping: register -> resolve consumes exactly once.
    #[tokio::test]
    async fn pending_resolution_is_single_shot() {
        let tagma = TagmaId::from("t-1".to_string());
        let (tx, rx) = oneshot::channel();
        pending().lock().await.insert((tagma.clone(), 42), tx);
        assert!(
            resolve(
                &tagma,
                42,
                ManageRestReply {
                    req_id: 42,
                    status: 200,
                    body: serde_json::json!({}),
                }
            )
            .await
        );
        // Second resolve: already consumed.
        assert!(
            !resolve(
                &tagma,
                42,
                ManageRestReply {
                    req_id: 42,
                    status: 200,
                    body: serde_json::json!({}),
                }
            )
            .await
        );
        let _ = rx.await;
    }

    /// Tenant nail: pending keys are tenant-bound -- tagma B cannot resolve
    /// tagma A's in-flight request even for the same req_id.
    #[tokio::test]
    async fn pending_resolution_is_tenant_bound() {
        let a = TagmaId::from("t-a".to_string());
        let b = TagmaId::from("t-b".to_string());
        let (tx, rx) = oneshot::channel();
        pending().lock().await.insert((a.clone(), 9), tx);
        assert!(
            !resolve(
                &b,
                9,
                ManageRestReply {
                    req_id: 9,
                    status: 200,
                    body: serde_json::json!({})
                },
            )
            .await
        );
        assert!(
            resolve(
                &a,
                9,
                ManageRestReply {
                    req_id: 9,
                    status: 200,
                    body: serde_json::json!({})
                },
            )
            .await
        );
        let _ = rx.await;
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use crate::routes::test_support::db_state;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{Method, StatusCode, Uri};
    use kallip_archeion_common::ids::UserId;
    use std::str::FromStr;
    use tower::ServiceExt;

    fn uid(s: &str) -> UserId {
        UserId::from(s.to_string())
    }

    fn uri_of(s: &str) -> Uri {
        Uri::from_str(s).expect("test uri")
    }

    /// Direct handler call with pre-built extractors, in the handler's
    /// parameter order (Path first). This leg covers authorization and
    /// frame semantics; the router-level shape pin below covers the
    /// route/extractor capture contract, which a direct call bypasses.
    async fn call(
        state: &SharedConvState,
        user: &UserId,
        agent: &str,
        method: &str,
        uri: &Uri,
        body: Option<serde_json::Value>,
    ) -> axum::response::Response {
        proxy_manage(
            State(state.clone()),
            // Mirrors the wildcard capture: the relative path exactly as
            // the router hands it over (no leading slash).
            Path((
                agent.to_string(),
                uri.path().trim_start_matches('/').to_string(),
            )),
            AuthPrincipal(Principal::User(user.clone())),
            Method::from_bytes(method.as_bytes()).expect("valid method"),
            uri.clone(),
            body.map(axum::Json),
        )
        .await
    }

    async fn enroll_tunnel(
        state: &SharedConvState,
        tagma: &TagmaId,
        owner: &UserId,
    ) -> tokio::sync::broadcast::Receiver<TunnelInbound> {
        let mut reg = state.registry.write().unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        reg.register_presence(tagma, owner.clone(), tx, std::sync::Arc::new(()));
        rx
    }

    fn status_of(resp: &axum::response::Response) -> StatusCode {
        resp.status()
    }

    #[tokio::test]
    async fn cross_tenant_request_is_forbidden() {
        let (state, _control) = db_state().await;
        let tagma = TagmaId::from("t-a".to_string());
        let owner = uid("alice");
        let other = uid("mallory");
        let _rx = enroll_tunnel(&state, &tagma, &owner).await;
        let resp = call(&state, &other, "t-a", "GET", &uri_of("/agents"), None).await;
        assert_eq!(status_of(&resp), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn owner_request_fans_uppercase_frame_and_resolves() {
        let (state, _control) = db_state().await;
        let tagma = TagmaId::from("t-a".to_string());
        let owner = uid("alice");
        let mut rx = enroll_tunnel(&state, &tagma, &owner).await;
        let resolver = {
            let tagma = tagma.clone();
            async move {
                let frame = rx.recv().await.expect("frame fanned");
                match frame {
                    TunnelInbound::ManageRest { req_id, method, .. } => {
                        assert_eq!(method, "GET"); // upper-case contract
                        crate::routes::manage_proxy::resolve(
                            &tagma,
                            req_id,
                            ManageRestReply {
                                req_id,
                                status: 200,
                                body: serde_json::json!({}),
                            },
                        )
                        .await;
                    }
                    _ => panic!("expected ManageRest"),
                }
            }
        };
        // join!: drives the handler and the frame resolver concurrently so
        // the reply round-trip cannot deadlock on a single-threaded test
        // runtime.
        let uri = uri_of("/agents");
        let (resp, ()) = tokio::join!(
            call(
                &state, &owner, "t-a", "get", // lower-case input must be normalized
                &uri, None,
            ),
            resolver
        );
        assert_eq!(status_of(&resp), StatusCode::OK);
    }

    /// Nail: `?include=` rides the frame BODY (the frame path stays
    /// query-free) and every frame carries a fresh UUID trace, so traces
    /// from two requests never collide even across reconnects.
    #[tokio::test]
    async fn include_query_rides_body_with_unique_traces() {
        let (state, _control) = db_state().await;
        let tagma = TagmaId::from("t-a".to_string());
        let owner = uid("alice");
        let mut rx = enroll_tunnel(&state, &tagma, &owner).await;
        let uri = uri_of("/agents?include=status");
        let first = tokio::join!(call(&state, &owner, "t-a", "GET", &uri, None), async {
            match rx.recv().await.expect("frame 1") {
                TunnelInbound::ManageRest {
                    req_id,
                    method: _,
                    path,
                    body,
                    trace,
                } => {
                    assert_eq!(path, "/agents");
                    assert_eq!(body["include"], serde_json::json!(["status"]));
                    assert!(!trace.as_ref().is_empty());
                    crate::routes::manage_proxy::resolve(
                        &tagma,
                        req_id,
                        ManageRestReply {
                            req_id,
                            status: 200,
                            body: serde_json::json!({}),
                        },
                    )
                    .await;
                    trace
                }
                _ => panic!("expected ManageRest"),
            }
        });
        assert_eq!(status_of(&first.0), StatusCode::OK);
        let second = tokio::join!(call(&state, &owner, "t-a", "GET", &uri, None), async {
            match rx.recv().await.expect("frame 2") {
                TunnelInbound::ManageRest { req_id, trace, .. } => {
                    crate::routes::manage_proxy::resolve(
                        &tagma,
                        req_id,
                        ManageRestReply {
                            req_id,
                            status: 200,
                            body: serde_json::json!({}),
                        },
                    )
                    .await;
                    trace
                }
                _ => panic!("expected ManageRest"),
            }
        });
        assert_eq!(status_of(&second.0), StatusCode::OK);
        assert_ne!(first.1, second.1, "traces must be unique per request");
    }
    #[tokio::test]
    async fn offline_tagma_is_not_found() {
        let (state, _control) = db_state().await;
        let resp = call(
            &state,
            &uid("alice"),
            "t-ghost",
            "GET",
            &uri_of("/agents"),
            None,
        )
        .await;
        assert_eq!(status_of(&resp), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn query_string_is_rejected() {
        let (state, _control) = db_state().await;
        let tagma = TagmaId::from("t-a".to_string());
        let owner = uid("alice");
        let _rx = enroll_tunnel(&state, &tagma, &owner).await;
        let uri = Uri::from_static("/agents?state=idle");
        let resp = call(&state, &owner, "t-a", "GET", &uri, None).await;
        assert_eq!(status_of(&resp), StatusCode::NOT_FOUND);
    }
    /// Production-incident nail (the proxy 500): every proxied manage
    /// path shape must survive the router's Path extractor. The request
    /// goes through the real router UNAUTHENTICATED: the handler takes
    /// Path before AuthPrincipal, so a 401 here proves the capture set
    /// matched (a shape mismatch would 500 first); no credentials are
    /// needed and no tunnel waiter can hang. Single-segment,
    /// parameterized, and two-parameter paths are all covered.
    #[tokio::test]
    async fn proxied_paths_pass_the_path_extractor_before_auth() {
        let (state, _control) = db_state().await;
        let shapes = [
            "/agents",
            "/agents/a-1/status",
            "/agents/a-1/lesche/direct-sessions/peer-9/messages",
        ];
        for path in shapes {
            let full_uri = format!("/tagmata/t-a/manage{path}");
            let request = Request::builder()
                .method(Method::GET)
                .uri(full_uri)
                .body(Body::empty())
                .expect("static request");
            let resp = router()
                .with_state(state.clone())
                .oneshot(request)
                .await
                .expect("infallible oneshot");
            assert_ne!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "path-shape rejection on {path}"
            );
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "auth must be the only rejecter left on {path}"
            );
        }
    }
}
