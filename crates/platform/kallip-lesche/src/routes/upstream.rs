//! The single upstream channel: `POST /v1/tagmata/{tagma_id}/upstream`.
//!
//! One authenticated endpoint carries a batched `Vec<UpstreamEvent>` of the
//! three plaintext metadata kinds (status / signal / state).
//! The tagma's upstream flusher serializes its bus topics into the
//! wire enum; here each element demultiplexes into its fan logic. The
//! per-kind shapes are nailed dead by the route-shape negative legs in
//! `routes.rs`.
//!
//! Batch semantics: elements are applied in order; the first failing
//! element aborts the batch with that element's error (earlier elements
//! stay applied — every fan here is idempotent or latest-wins, so a
//! flushing retry of the retained keep-latest set is safe). The response
//! counts applied elements; an empty batch is a no-op `{"applied": 0}`.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use kallip_archeion_common::ids::TagmaId;
use kallip_common::protocol::ApiError;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::state::SharedConvState;
use kallip_lesche_common::event::UpstreamEvent;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tagmata/{tagma_id}/upstream", post(post_upstream))
}

/// `POST /v1/tagmata/{tagma_id}/upstream` — apply a batch of plaintext
/// metadata events on behalf of the authenticated tagma. The path `tagma_id`
/// is authoritative (matched against the authenticated tagma, as on the
/// per-kind endpoints); one check covers the whole batch.
async fn post_upstream(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(tagma_id): Path<String>,
    Json(events): Json<Vec<UpstreamEvent>>,
) -> Result<Response, ApiError> {
    let path_tagma = TagmaId::from(tagma_id);
    let authed_tagma = require_tagma(&principal)?;
    if &path_tagma != authed_tagma {
        return Err(ApiError::forbidden("upstream tagma_id does not match auth"));
    }

    let mut applied = 0usize;
    for event in events {
        match event {
            UpstreamEvent::Status(payload) => {
                super::status::relay_status(&state, path_tagma.clone(), payload).await?;
                applied += 1;
            }
            UpstreamEvent::Signal(event) => {
                super::signal::relay_signal(&state, path_tagma.clone(), event).await?;
                applied += 1;
            }
            UpstreamEvent::Projection(snapshot) => {
                let response =
                    super::state::accept_projection(&state, path_tagma.clone(), *snapshot).await;
                if response.status() != StatusCode::OK {
                    return Ok(response);
                }
                applied += 1;
            }
        }
    }
    // The piggyback per-face counts are read AFTER the batch is
    // applied -- the freshest per-face truth, so the tagma can reconcile
    // its gates against them. Early-error responses (403/404) carry no
    // counts: no POST succeeded, nothing to reconcile on.
    let faces = {
        let reg = state
            .read()
            .map_err(|poisoned| ApiError::internal(format_args!("registry error: {poisoned}")))?;
        let status_live = reg
            .presence_by_tagma(&path_tagma)
            .map(|entry| reg.has_app_stream(&entry.owner))
            .unwrap_or(false);
        serde_json::json!({
            "status": status_live as u64,
            "projection": reg.projection_stream_live_for_tagma(&path_tagma) as u64,
        })
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "applied": applied, "faces": faces })),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_archeion_common::bytes::Ed25519PublicKey;
    use kallip_archeion_common::ids::UserId;
    use kallip_archeion_common::principal::Principal;
    use kallip_common::protocol::{AgentState, SignalEvent};
    use kallip_lesche_common::event::LescheEvent;
    use kallip_lesche_common::event::{TagmaStatusPayload, UpstreamEvent};
    use kallip_lesche_common::projection::ProjectionSnapshot;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    fn status_payload() -> TagmaStatusPayload {
        TagmaStatusPayload {
            root_state: AgentState::Busy,
            subagents_total: 3,
            subagents_active: 2,
            token_budget: 50_000,
            token_consumed: 12_000,
            token_budget_unlimited: false,
        }
    }

    fn projection_snapshot() -> ProjectionSnapshot {
        serde_json::from_value(serde_json::json!({
            "agents": [],
            "status": {
                "root_state": "idle",
                "subagents_total": 0,
                "subagents_active": 0,
                "token_budget": 1,
                "token_consumed": 0,
            },
            "push_seq": 7,
            "work_schedule": null,
        }))
        .expect("snapshot parses")
    }

    /// One POST carries all three plaintext metadata kinds; each element
    /// fans exactly like its per-kind endpoint (status broadcast + signal
    /// rebroadcast on the owner's app stream, projection into the store).
    #[tokio::test]
    async fn mixed_batch_demuxes_to_per_kind_fans() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let app_tx = state.write().unwrap().open_app_stream(&owner);
        let mut rx = app_tx.subscribe();
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());

        let response = post_upstream(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(vec![
                UpstreamEvent::Status(status_payload()),
                UpstreamEvent::Signal(SignalEvent::Busy),
                UpstreamEvent::Projection(Box::new(projection_snapshot())),
            ]),
        )
        .await
        .expect("batch ok");
        assert_eq!(response.status(), StatusCode::OK);

        // Status and signal arrive on the owner stream, in batch order.
        match rx.recv().await.expect("event delivered").event {
            LescheEvent::TagmaStatus {
                tagma_id,
                root_state,
                ..
            } => {
                assert_eq!(tagma_id, tagma);
                assert_eq!(root_state, AgentState::Busy);
            }
            other => panic!("expected TagmaStatus first, got {other:?}"),
        }
        match rx.recv().await.expect("event delivered").event {
            LescheEvent::TagmaSignal { tagma_id, event } => {
                assert_eq!(tagma_id, tagma);
                assert_eq!(event, SignalEvent::Busy);
            }
            other => panic!("expected TagmaSignal second, got {other:?}"),
        }
        // The projection element landed in the store (same fan as state PUT).
        assert!(
            state
                .registry
                .read()
                .expect("registry lock")
                .projection(&tagma)
                .is_some()
        );
    }

    /// The piggyback per-face counts: an OK response reports the
    /// owner's live app-stream existence and the tagma's live projection
    /// channels, read AFTER the batch applied. Early-error responses (the
    /// 403/404 legs below) carry no body at all, hence no counts.
    #[tokio::test]
    async fn ok_response_carries_piggyback_face_counts() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let _tunnel = seed_presence(&state, &tagma, owner.clone());
        let post = |state: SharedConvState| {
            let tagma = tagma.clone();
            async move {
                let path = Path(tagma.to_string());
                post_upstream(
                    State(state),
                    AuthPrincipal(Principal::Tagma(tagma)),
                    path,
                    Json(vec![UpstreamEvent::Status(status_payload())]),
                )
                .await
                .expect("batch ok")
            }
        };
        // No app stream: the status face reads 0 despite the applied event.
        let response = post(state.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["faces"]["status"], serde_json::json!(0));
        assert_eq!(json["faces"]["projection"], serde_json::json!(0));

        // The 0 -> 1 edge (the create fans its hint) flips the count.
        let app = state.write().unwrap().open_app_stream(&owner);
        let _rx = app.subscribe();
        let response = post(state.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["faces"]["status"],
            serde_json::json!(1),
            "live app stream reads 1"
        );
        assert_eq!(json["faces"]["projection"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn empty_batch_is_a_no_op_ok() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let tagma = TagmaId::from("tagma-1".to_string());
        let response = post_upstream(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(vec![]),
        )
        .await
        .expect("empty batch ok");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_upstream_auth_mismatch_403() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let tagma = TagmaId::from("tagma-1".to_string());
        let other = TagmaId::from("tagma-2".to_string());
        control.enroll_tagma(
            &tagma,
            user("owner"),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        control.enroll_tagma(
            &other,
            user("someone"),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let err = post_upstream(
            State(state),
            AuthPrincipal(Principal::Tagma(other)),
            Path(tagma.to_string()),
            Json(vec![UpstreamEvent::Status(status_payload())]),
        )
        .await
        .expect_err("mismatch 403");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn post_upstream_no_live_tunnel_404() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let tagma = TagmaId::from("ghost".to_string());
        let err = post_upstream(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(vec![UpstreamEvent::Status(status_payload())]),
        )
        .await
        .expect_err("no tunnel 404");
        assert_eq!(err.status, 404);
    }
}
