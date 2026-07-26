//! Tagma status relay: `POST /v1/tagmata/{tagma_id}/status`.
//!
//! The tagma periodically snapshots its aggregate runtime state (agent counts
//! and token budget) and POSTs it here; the lesche rebroadcasts it as an
//! [`AgoraEvent::TagmaStatus`] on the owner's app event stream. Like presence,
//! status is plaintext and user-scoped, so the lesche can read it -- agent
//! counts and token budget are operator metadata, not conversation content.
//! The relay does not parse or validate the numbers and does not rate-limit;
//! the snapshot cadence is a tagma-side contract.
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous), never co-held with a `ControlPlane` call.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use kallip_agora_common::event::{AgoraEvent, TagmaStatusPayload};
use kallip_agora_common::ids::TagmaId;
use kallip_common::protocol::ApiError;
use tracing::debug;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tagmata/{tagma_id}/status", post(post_status))
}

/// `POST /v1/tagmata/{tagma_id}/status` -- rebroadcast the tagma's aggregate
/// runtime snapshot to its owner's app event stream. The path `tagma_id` is
/// authoritative (matched against the authenticated tagma); the body carries
/// only the counts/budget. If the owner has no live app stream the snapshot is
/// silently dropped (no client listening) -- the next tick supersedes it, so
/// best-effort delivery is sufficient and the tagma must not retry.
async fn post_status(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(tagma_id): Path<String>,
    Json(payload): Json<TagmaStatusPayload>,
) -> Result<StatusCode, ApiError> {
    let path_tagma = TagmaId::from(tagma_id);
    let authed_tagma = require_tagma(&principal)?;
    if &path_tagma != authed_tagma {
        return Err(ApiError::forbidden("status tagma_id does not match auth"));
    }

    // Resolve the owner from the in-memory presence cache (populated on tunnel
    // open by `register_presence`). The status pump runs only while the tunnel
    // is live, so presence is guaranteed present; a missing entry means the
    // tunnel is gone -- surface 404 rather than silently masking a routing
    // gap. Guard is dropped before any await (lock discipline invariant #1).
    let app_tx = {
        let reg = state.read()?;
        let owner = reg
            .presence
            .get(&path_tagma)
            .ok_or_else(|| ApiError::not_found("no live tunnel for tagma"))?
            .owner
            .clone();
        reg.app_stream(&owner).cloned()
    };

    // No live app stream -> silent drop (best-effort). Still 202 so the tagma
    // does not retry; the next periodic snapshot supersedes this one.
    if let Some(tx) = app_tx
        && tx
            .send(AgoraEvent::TagmaStatus {
                tagma_id: path_tagma.clone(),
                root_state: payload.root_state,
                subagents_total: payload.subagents_total,
                subagents_active: payload.subagents_active,
                token_budget: payload.token_budget,
                token_consumed: payload.token_consumed,
            })
            .is_ok()
    {
        debug!(tagma = %path_tagma, "status relayed");
    }
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_agora_common::bytes::Ed25519PublicKey;
    use kallip_agora_common::event::AgoraEvent;
    use kallip_agora_common::ids::{TagmaId, UserId};
    use kallip_agora_common::principal::Principal;
    use kallip_common::protocol::AgentState;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    #[tokio::test]
    async fn post_status_relays_to_owner_app_stream() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        // Presence alone is not enough -- the owner also needs an open app
        // stream (created by `me_events` in production).
        let app_tx = state.write().unwrap().open_app_stream(&owner);
        let mut rx = app_tx.subscribe();
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());

        let status = post_status(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(TagmaStatusPayload {
                root_state: AgentState::Busy,
                subagents_total: 3,
                subagents_active: 2,
                token_budget: 50_000,
                token_consumed: 12_000,
            }),
        )
        .await
        .expect("relay ok");
        assert_eq!(status, StatusCode::ACCEPTED);

        match rx.recv().await.expect("event delivered") {
            AgoraEvent::TagmaStatus {
                tagma_id,
                root_state,
                subagents_total,
                subagents_active,
                token_budget,
                token_consumed,
            } => {
                assert_eq!(tagma_id, tagma);
                assert_eq!(root_state, AgentState::Busy);
                assert_eq!((subagents_total, subagents_active), (3, 2));
                assert_eq!((token_budget, token_consumed), (50_000, 12_000));
            }
            other => panic!("expected TagmaStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_status_auth_mismatch_403() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
        seed_presence(&state, &tagma, UserId::from("owner".to_string()));

        let err = post_status(
            State(state),
            AuthPrincipal(Principal::Tagma(TagmaId::from("tagma-other".to_string()))),
            Path(tagma.to_string()),
            Json(TagmaStatusPayload {
                root_state: AgentState::Idle,
                subagents_total: 1,
                subagents_active: 0,
                token_budget: 0,
                token_consumed: 0,
            }),
        )
        .await
        .expect_err("mismatch 403");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn post_status_no_live_tunnel_404() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let tagma = TagmaId::from("ghost".to_string());
        let err = post_status(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(TagmaStatusPayload {
                root_state: AgentState::Idle,
                subagents_total: 0,
                subagents_active: 0,
                token_budget: 0,
                token_consumed: 0,
            }),
        )
        .await
        .expect_err("no tunnel 404");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn post_status_silent_drop_when_no_app_stream() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
        // Presence but NO open app stream -- the owner is offline.
        seed_presence(&state, &tagma, UserId::from("owner".to_string()));

        let status = post_status(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path("tagma-1".to_string()),
            Json(TagmaStatusPayload {
                root_state: AgentState::Busy,
                subagents_total: 1,
                subagents_active: 1,
                token_budget: 100,
                token_consumed: 1,
            }),
        )
        .await
        .expect("silent drop still 202");
        assert_eq!(status, StatusCode::ACCEPTED);
    }
}
