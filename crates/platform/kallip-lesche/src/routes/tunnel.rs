//! The tunnel: a long-lived SSE the tagma holds open to receive forwarded
//! envelopes. Establishing it (with a fresh signed proof of the pinned device
//! key) marks the tagma online and pushes `TagmaOnline` to the owner's app
//! stream; disconnect removes presence (only if this tunnel is still the live
//! one) and pushes `TagmaOffline`. A second concurrent tunnel for one tagma is
//! rejected.
//!
//! Every (re)connect must present `X-Device-Timestamp` + `X-Device-Proof`: an
//! Ed25519 signature over the tunnel transcript, verified against the tagma's
//! pinned key, with the timestamp within `+/- proof_skew_secs`. The pinned key
//! and the durable replay guard are fetched/advanced through the registry's
//! `ControlPlane` trait.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use kallip_agora_common::ids::ParticipantId;
use kallip_agora_common::proof::ProofError;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::event::LescheEvent;
use kallip_lesche_common::proof::verify_tunnel_proof;
use kallip_lesche_common::tunnel::TunnelInbound;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::{AgentProfile, BROADCAST_CAPACITY, SharedConvState};

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tunnel", get(tunnel))
}

/// Wall-clock unix seconds, for proof skew checks.
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn tunnel(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: axum::http::HeaderMap,
) -> Result<Sse<OnDrop>, ApiError> {
    let tagma_id = require_tagma(&principal)?.clone();
    // Capture the runtime handle so the synchronous `OnDrop` cleanup (which may
    // run off-runtime, e.g. during body teardown) can spawn the presence
    // fan-out without `tokio::spawn`'s implicit `Handle::current()` panic.
    let handle = tokio::runtime::Handle::try_current().ok();

    // Proof of possession: timestamp within the skew window + signature over the
    // tunnel transcript.
    let ts: i64 = headers
        .get("X-Device-Timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ApiError::bad_request("missing or malformed X-Device-Timestamp"))?;
    let now = now_unix_secs();
    if (now - ts).abs() > state.proof_skew_secs {
        return Err(ApiError::unauthorized(
            "device proof timestamp outside the skew window",
        ));
    }
    let sig_bytes = headers
        .get("X-Device-Proof")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| STANDARD.decode(s).ok())
        .ok_or_else(|| ApiError::bad_request("missing or malformed X-Device-Proof"))?;
    // The pinned device key + owner come from the registry's raw tagma facts,
    // fetched outside the relay lock. The usability gate (enrolled + non-revoked
    // + pinned key) is derived locally; any failure -- unknown tagma, pending,
    // revoked -- collapses to one "unknown tagma" 404 (the existence-oracle).
    let profile = crate::control_policy::tagma_profile(&*state.control, &tagma_id)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?;
    let identity = profile
        .as_ref()
        .filter(|p| crate::control_policy::tunnel_usable(p))
        .and_then(|p| {
            // A usable tagma has a pinned key by construction (tunnel_usable).
            p.pinned_public_key.clone().map(|key| (p, key))
        })
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    let (profile, pinned_public_key) = identity;
    verify_tunnel_proof(&pinned_public_key.0, tagma_id.as_ref(), ts, &sig_bytes)
        .map_err(proof_to_unauthorized)?;

    // Cache the authoritative agent profile (label + owner display) so the
    // rooms send path stamps the sender handle without a per-message registry
    // call. Refreshed on every (re)connect, so a rename lands at the next
    // tunnel reconnect.
    state.agent_profiles.set(
        ParticipantId::for_tagma(&tagma_id),
        AgentProfile {
            label: profile.label.clone(),
            owner_username: profile.owner_username.clone(),
            owner_display_name: profile.owner_display_name.clone(),
        },
    );

    // Durable replay guard: accept this proof only if the tagma's stored
    // high-water-mark timestamp advanced. Cross-restart, atomic.
    let fresh = state
        .control
        .bump_tunnel_proof_ts(&tagma_id, ts)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?;
    if !fresh {
        return Err(ApiError::unauthorized("replayed or stale device proof"));
    }

    // Reserve the tunnel slot and announce presence. One live tunnel per tagma.
    let owner = profile.owner_user_id.clone();
    let (tx, rx) = broadcast::channel::<TunnelInbound>(BROADCAST_CAPACITY);
    let id = Arc::new(());
    {
        let mut reg = state.write()?;
        if reg.presence_by_tagma(&tagma_id).is_some() {
            return Err(ApiError::conflict("tagma already has a live tunnel"));
        }
        reg.register_presence(&tagma_id, owner.clone(), tx.clone(), id.clone());
        // Announce online to the owner's app stream, if one is open. The tunnel
        // never *creates* an app stream (only `me_events` may); if the owner is
        // not connected now, they get this tagma in their snapshot on connect.
        if let Some(app_tx) = reg.app_stream(&owner) {
            let _ = app_tx.send(LescheEvent::TagmaOnline {
                tagma_id: tagma_id.clone(),
            });
        }
    }
    // Announce room-member presence to peers (best-effort, off the request path).
    // Presence is soft state; a dropped frame self-heals on the viewer's roster
    // re-fetch (the roster's `online` field reads the live registry).
    if let Some(h) = &handle {
        let st = state.clone();
        let who = ParticipantId::for_tagma(&tagma_id);
        h.spawn(async move {
            crate::room_presence::fan_member_presence(&st, &who, true).await;
        });
    }
    tracing::info!(tagma = %tagma_id, "tunnel established; tagma online");

    let lag_tagma = tagma_id.clone();
    let stream: BoxEventStream = Box::pin(
        BroadcastStream::new(rx)
            .filter_map(move |r| match r {
                Ok(env) => Some(env),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(lag = n, tagma = %lag_tagma, "tunnel lagged; envelopes dropped");
                    None
                }
            })
            .map(|env| {
                Ok::<Event, std::convert::Infallible>(
                    Event::default()
                        .json_data(env)
                        .expect("envelope serializes"),
                )
            }),
    );

    // Synchronous cleanup in Drop::drop: remove presence only if this tunnel is
    // still the live one, and announce offline to the owner. The lock guard is
    // dropped before the (async) room-presence fan-out is spawned off-thread.
    let cleanup_state = state.clone();
    let cleanup_tagma = tagma_id.clone();
    let cleanup_owner = owner.clone();
    let cleanup_id = id.clone();
    let cleanup_handle = handle.clone();
    let cleaned = OnDrop::new(stream, move || {
        let removed = {
            let Ok(mut reg) = cleanup_state.write() else {
                return;
            };
            let removed = reg.take_presence_if_owned(&cleanup_tagma, &cleanup_id);
            if removed {
                if let Some(app_tx) = reg.app_stream(&cleanup_owner) {
                    let _ = app_tx.send(LescheEvent::TagmaOffline {
                        tagma_id: cleanup_tagma.clone(),
                    });
                }
                tracing::info!(tagma = %cleanup_tagma, "tunnel closed; presence removed");
            }
            removed
        };
        if removed {
            if let Some(h) = cleanup_handle.as_ref() {
                let st = cleanup_state.clone();
                let who = ParticipantId::for_tagma(&cleanup_tagma);
                h.spawn(async move {
                    crate::room_presence::fan_member_presence(&st, &who, false).await;
                });
            } else {
                tracing::warn!("offline-presence fan skipped: no runtime at drop");
            }
        }
    });
    Ok(Sse::new(cleaned))
}

/// A rejected tunnel proof is an auth failure.
fn proof_to_unauthorized(e: ProofError) -> ApiError {
    ApiError::unauthorized(format!("invalid device proof: {e}"))
}

#[cfg(test)]
mod tests {
    //! The tunnel-establish gate (tagma facts + usability predicate) runs BEFORE
    //! the Ed25519 proof check, so the existence-oracle 404s can be exercised
    //! without a valid signature.

    use super::*;
    use crate::test_support::make_state;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use kallip_agora_common::bytes::Ed25519PublicKey;
    use kallip_agora_common::ids::{TagmaId, UserId};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ts() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Headers carrying a skew-ok timestamp + a parseable (unverified) base64
    /// signature: enough to reach the gate, never the proof check.
    fn gate_headers(ts: i64) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("X-Device-Timestamp", ts.to_string().parse().unwrap());
        h.insert(
            "X-Device-Proof",
            STANDARD.encode([0u8; 64]).parse().unwrap(),
        );
        h
    }

    fn as_tagma(t: &TagmaId) -> AuthPrincipal {
        AuthPrincipal(kallip_agora_common::principal::Principal::Tagma(t.clone()))
    }

    /// The gate collapses unknown / revoked / pending (no pinned key) tagmas to
    /// one byte-identical "unknown tagma" 404, before the proof check.
    #[tokio::test]
    async fn tunnel_gate_oracle_is_uniform_across_failure_modes() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = UserId::from("owner".to_string());
        let revoked = TagmaId::from("rev".to_string());
        control.enroll_tagma(
            &revoked,
            owner.clone(),
            Ed25519PublicKey(vec![1u8; 32]),
            "tok-rev",
        );
        control.revoke_tagma(&revoked);
        let pending = TagmaId::from("pen".to_string());
        control.enroll_tagma(&pending, owner, Ed25519PublicKey(vec![2u8; 32]), "tok-pen");
        control.set_pinned_key(&pending, None);
        let unknown = TagmaId::from("ghost".to_string());

        let ts = now_ts();
        let e_unknown = tunnel(State(state.clone()), as_tagma(&unknown), gate_headers(ts))
            .await
            .expect_err("unknown tagma");
        let e_revoked = tunnel(State(state.clone()), as_tagma(&revoked), gate_headers(ts))
            .await
            .expect_err("revoked tagma");
        let e_pending = tunnel(State(state), as_tagma(&pending), gate_headers(ts))
            .await
            .expect_err("pending tagma");
        assert_eq!(e_unknown.status, 404);
        assert_eq!(e_unknown.message, e_revoked.message);
        assert_eq!(e_unknown.message, e_pending.message);
    }
}
