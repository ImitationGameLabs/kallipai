//! The herald tunnel: a long-lived SSE the herald holds open to receive
//! forwarded envelopes. Establishing it (with a fresh signed proof of the pinned
//! device key) marks the tagma online and pushes `TagmaOnline` to the owner's
//! app stream; disconnect removes presence (only if this tunnel is still the
//! live one) and pushes `TagmaOffline`. A second concurrent tunnel for one
//! tagma is rejected.
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
use kallip_agora_common::event::AgoraEvent;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::proof::{ProofError, verify_tunnel_proof};
use kallip_common::protocol::ApiError;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::{BROADCAST_CAPACITY, SharedConvState};

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
    // The pinned device key + owner come from the registry. Fetched outside the
    // relay lock. A missing tagma or a still-pending tagma (no pinned key) is
    // "unknown tagma".
    let identity = state
        .control
        .tagma_identity(&tagma_id)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .ok_or_else(|| ApiError::not_found("unknown tagma"))?;
    verify_tunnel_proof(
        &identity.pinned_public_key.0,
        tagma_id.as_ref(),
        ts,
        &sig_bytes,
    )
    .map_err(proof_to_unauthorized)?;

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
    let owner = identity.owner_user_id.clone();
    let (tx, rx) = broadcast::channel::<HeraldInbound>(BROADCAST_CAPACITY);
    let id = Arc::new(());
    {
        let mut reg = state.write()?;
        if reg.presence.contains_key(&tagma_id) {
            return Err(ApiError::conflict("tagma already has a live tunnel"));
        }
        reg.register_presence(&tagma_id, owner.clone(), tx, id.clone());
        // Announce online to the owner's app stream, if one is open. The tunnel
        // never *creates* an app stream (only `me_events` may); if the owner is
        // not connected now, they get this tagma in their snapshot on connect.
        if let Some(app_tx) = reg.app_stream(&owner) {
            let _ = app_tx.send(AgoraEvent::TagmaOnline {
                tagma_id: tagma_id.clone(),
            });
        }
    }
    tracing::info!(tagma = %tagma_id, "herald tunnel established; tagma online");

    let lag_tagma = tagma_id.clone();
    let stream: BoxEventStream =
        Box::pin(BroadcastStream::new(rx).filter_map(move |r| match r {
            Ok(env) => Some(env),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                tracing::warn!(lag = n, tagma = %lag_tagma, "herald tunnel lagged; envelopes dropped");
                None
            }
        }).map(|env| {
            Ok::<Event, std::convert::Infallible>(
                Event::default()
                    .json_data(env)
                    .expect("envelope serializes"),
            )
        }));

    // Synchronous cleanup in Drop::drop: remove presence only if this tunnel is
    // still the live one, and announce offline to the owner.
    let cleanup_state = state.clone();
    let cleanup_tagma = tagma_id.clone();
    let cleanup_owner = owner.clone();
    let cleanup_id = id.clone();
    let cleaned = OnDrop::new(stream, move || {
        let Ok(mut reg) = cleanup_state.write() else {
            return;
        };
        if reg.take_presence_if_owned(&cleanup_tagma, &cleanup_id) {
            if let Some(app_tx) = reg.app_stream(&cleanup_owner) {
                let _ = app_tx.send(AgoraEvent::TagmaOffline {
                    tagma_id: cleanup_tagma.clone(),
                });
            }
            tracing::info!(tagma = %cleanup_tagma, "herald tunnel closed; presence removed");
        }
    });
    Ok(Sse::new(cleaned))
}

/// A rejected tunnel proof is an auth failure.
fn proof_to_unauthorized(e: ProofError) -> ApiError {
    ApiError::unauthorized(format!("invalid device proof: {e}"))
}
