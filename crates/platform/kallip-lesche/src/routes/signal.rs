//! Tagma signal relay: `POST /v1/tagmata/{tagma_id}/signal`.
//!
//! The tagma pushes a per-event runtime signal (busy/idle presence, turn
//! terminals, errors) here; the lesche rebroadcasts it as a
//! [`LescheEvent::TagmaSignal`] on the owner's app event stream. Like status
//! and presence, the signal is plaintext and user-scoped, so the lesche can
//! read it — these are operator metadata, not conversation content (authored
//! content rides the encrypted envelope). The relay does not interpret the
//! event and does not rate-limit; the tagma also writes each to its own
//! application log for observability.
//!
//! Signals are not persisted in `chat_history` and not replayed: a reconnect
//! only replays authored messages. If the owner has no live app stream the
//! signal is silently dropped (best-effort), still 202.
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous), never co-held with a `ControlPlane` call.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use kallip_agora_common::ids::TagmaId;
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SignalEvent;
use kallip_lesche_common::event::LescheEvent;
use tracing::debug;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tagmata/{tagma_id}/signal", post(post_signal))
}

/// `POST /v1/tagmata/{tagma_id}/signal` -- rebroadcast a tagma runtime signal
/// to its owner's app event stream. The path `tagma_id` is authoritative
/// (matched against the authenticated tagma); the body carries the
/// [`SignalEvent`]. Silent-drop (still 202) when the owner has no live app
/// stream.
async fn post_signal(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(tagma_id): Path<String>,
    Json(event): Json<SignalEvent>,
) -> Result<StatusCode, ApiError> {
    let path_tagma = TagmaId::from(tagma_id);
    let authed_tagma = require_tagma(&principal)?;
    if &path_tagma != authed_tagma {
        return Err(ApiError::forbidden("signal tagma_id does not match auth"));
    }

    let app_tx = {
        let reg = state.read()?;
        let owner = reg
            .presence_by_tagma(&path_tagma)
            .ok_or_else(|| ApiError::not_found("no live tunnel for tagma"))?
            .owner
            .clone();
        reg.app_stream(&owner).cloned()
    };

    if let Some(tx) = app_tx
        && tx
            .send(LescheEvent::TagmaSignal {
                tagma_id: path_tagma.clone(),
                event,
            })
            .is_ok()
    {
        debug!(tagma = %path_tagma, "signal relayed");
    }
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_agora_common::bytes::Ed25519PublicKey;
    use kallip_agora_common::ids::{TagmaId, UserId};
    use kallip_agora_common::principal::Principal;
    use kallip_common::protocol::SignalEvent;
    use kallip_lesche_common::event::LescheEvent;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    #[tokio::test]
    async fn post_signal_relays_to_owner_app_stream() {
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

        let status = post_signal(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(SignalEvent::Busy),
        )
        .await
        .expect("relay ok");
        assert_eq!(status, StatusCode::ACCEPTED);

        match rx.recv().await.expect("event delivered") {
            LescheEvent::TagmaSignal { tagma_id, event } => {
                assert_eq!(tagma_id, tagma);
                assert!(matches!(event, SignalEvent::Busy));
            }
            other => panic!("expected TagmaSignal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_signal_auth_mismatch_403() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
        seed_presence(&state, &tagma, UserId::from("owner".to_string()));

        let err = post_signal(
            State(state),
            AuthPrincipal(Principal::Tagma(TagmaId::from("tagma-other".to_string()))),
            Path(tagma.to_string()),
            Json(SignalEvent::Idle),
        )
        .await
        .expect_err("mismatch 403");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn post_signal_no_live_tunnel_404() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let tagma = TagmaId::from("ghost".to_string());
        let err = post_signal(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma.clone())),
            Path(tagma.to_string()),
            Json(SignalEvent::Interrupted),
        )
        .await
        .expect_err("no tunnel 404");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn post_signal_silent_drop_when_no_app_stream() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
        // Presence but NO open app stream -- the owner is offline.
        seed_presence(&state, &tagma, UserId::from("owner".to_string()));

        let status = post_signal(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path("tagma-1".to_string()),
            Json(SignalEvent::Busy),
        )
        .await
        .expect("silent drop still 202");
        assert_eq!(status, StatusCode::ACCEPTED);
    }
}
