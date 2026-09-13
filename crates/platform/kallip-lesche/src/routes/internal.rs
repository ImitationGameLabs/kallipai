//! Service-to-service event push surface (`/internal/*`), consumed by the
//! files service: a successful `POST /files/{id}/send` delivery pushes a
//! `FileDelivered` event onto the recipient's app event stream, so the
//! recipient's UI/agent learns a file arrived without polling. Mounted only
//! when the internal token is configured, behind `internal_guard` (the same
//! constant-time bearer-hash discipline as the archeion's internal surface).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use kallip_archeion_common::ids::UserId;
use kallip_lesche_common::event::LescheEvent;
use serde::Deserialize;

use crate::state::SharedConvState;
use kallip_common::protocol::ApiError;

/// The internal router: mounted under `/internal` only when the shared
/// secret is configured (see `crate::routes::router`).
pub fn router(state: SharedConvState) -> Router {
    Router::new()
        .route("/file-delivered", axum::routing::post(file_delivered))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
pub struct FileDeliveredRequest {
    /// The recipient user (the space owner the delivery landed in).
    pub to_user: String,
    pub record_id: uuid::Uuid,
    pub path: String,
    pub from: String,
    pub name: String,
    pub size: u64,
}

/// Push a `FileDelivered` event to the recipient's live app stream. A
/// recipient with no live stream answers `{"delivered": false}` with 200 --
/// the file itself is already safely in their files space; the event is an
/// optimization, not the delivery (the caller never retries).
pub async fn file_delivered(
    State(state): State<SharedConvState>,
    Json(req): Json<FileDeliveredRequest>,
) -> Result<Response, ApiError> {
    let user = UserId::from(req.to_user);
    let event = LescheEvent::FileDelivered {
        record_id: req.record_id,
        path: req.path,
        from: req.from,
        name: req.name,
        size: req.size,
    };
    let delivered = {
        let registry = state.read()?;
        match registry.app_stream(&user) {
            Some(stream) => stream.deliver(event).is_ok(),
            None => false,
        }
    };
    Ok(Json(serde_json::json!({ "delivered": delivered })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header::AUTHORIZATION, header::ORIGIN};
    use axum::middleware;
    use axum::routing::post;
    use kallip_common::authtoken::TokenHash;
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    fn app_with_stream(
        token: &str,
    ) -> (Router, broadcast::Receiver<crate::state::SequencedAppEvent>) {
        let (state, _control) = crate::test_support::make_state(60, Duration::from_secs(10));
        let user = UserId::from("u1".to_string());
        let stream = {
            let mut registry = state.write().unwrap();
            registry.open_app_stream(&user)
        };
        let rx = stream.subscribe();
        let app = Router::new()
            .route("/file-delivered", post(file_delivered))
            .with_state(state)
            .layer(middleware::from_fn_with_state(
                TokenHash::of(token),
                crate::middleware::internal_guard,
            ));
        (app, rx)
    }

    fn body() -> String {
        serde_json::json!({
            "to_user": "u1", "record_id": uuid::Uuid::nil(),
            "path": "/users/u1/inbox/a.txt", "from": "t1",
            "name": "a.txt", "size": 3
        })
        .to_string()
    }

    /// The bearer-hash gate: a wrong or missing token is 401 and must
    /// never reach the handler (and never broadcast).
    #[tokio::test]
    async fn rejects_a_wrong_or_missing_bearer() {
        let (app, mut rx) = app_with_stream("secret");
        for token in [None, Some("wrong")] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/file-delivered")
                .header(ORIGIN, "https://x");
            if let Some(t) = token {
                request = request.header(AUTHORIZATION, format!("Bearer {t}"));
            }
            let request = request.body(Body::from(body())).unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        assert!(rx.try_recv().is_err());
    }

    /// The happy face: the right token pushes the event to the live
    /// stream, and the recipient sees a well-formed FileDelivered.
    #[tokio::test]
    async fn delivers_the_event_to_the_live_stream() {
        let (app, mut rx) = app_with_stream("secret");
        let request = Request::builder()
            .method("POST")
            .uri("/file-delivered")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(body()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let event = rx.try_recv().expect("the event lands on the stream");
        match event.event {
            LescheEvent::FileDelivered {
                path, name, size, ..
            } => {
                assert_eq!(path, "/users/u1/inbox/a.txt");
                assert_eq!(name, "a.txt");
                assert_eq!(size, 3);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// No live app stream: the event is dropped (not queued) and the endpoint
    /// answers 200 with an honest `delivered: false`.
    #[tokio::test]
    async fn reports_delivered_false_without_a_live_stream() {
        let (state, _control) = crate::test_support::make_state(60, Duration::from_secs(10));
        let app = Router::new()
            .route("/file-delivered", post(file_delivered))
            .with_state(state)
            .layer(middleware::from_fn_with_state(
                TokenHash::of("secret"),
                crate::middleware::internal_guard,
            ));
        let request = Request::builder()
            .method("POST")
            .uri("/file-delivered")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, "Bearer secret")
            .body(Body::from(body()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"{\"delivered\":false}");
    }
}
