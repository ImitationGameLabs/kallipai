//! The herald tunnel: a long-lived SSE the herald holds open to receive
//! forwarded envelopes. Establishing it (with a fresh signed proof of the pinned
//! device key) marks the team online; disconnect removes presence only if this
//! tunnel is still the live one. A second concurrent tunnel for one team is
//! rejected.
//!
//! Every (re)connect must present `X-Device-Timestamp` + `X-Device-Proof`: an
//! Ed25519 signature over the tunnel transcript, verified against the team's
//! pinned key, with the timestamp within `+/- proof_skew_secs`. So a stolen
//! long-lived team token alone cannot open a tunnel.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::proof::{ProofError, verify_tunnel_proof};
use kallip_common::protocol::ApiError;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_team};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new().route("/tunnel", get(tunnel))
}

/// Wall-clock unix seconds from the agora's perspective, for proof skew checks.
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn tunnel(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    headers: axum::http::HeaderMap,
) -> Result<Sse<OnDrop>, ApiError> {
    let team_id = require_team(&principal)?.clone();

    // Proof of possession: timestamp within the skew window + signature over the
    // tunnel transcript, verified against the pinned key.
    let ts: i64 = headers
        .get("X-Device-Timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ApiError::bad_request("missing or malformed X-Device-Timestamp"))?;
    let now = now_unix_secs();
    if (now - ts).abs() > state.limits.proof_skew_secs {
        return Err(ApiError::unauthorized(
            "device proof timestamp outside the skew window",
        ));
    }
    let sig_bytes = headers
        .get("X-Device-Proof")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| STANDARD.decode(s).ok())
        .ok_or_else(|| ApiError::bad_request("missing or malformed X-Device-Proof"))?;
    let pinned = {
        let reg = state.read()?;
        let team = reg
            .teams
            .get(&team_id)
            .ok_or_else(|| ApiError::not_found("unknown team"))?;
        team.pinned_public_key.clone()
    };
    verify_tunnel_proof(&pinned.0, team_id.as_ref(), ts, &sig_bytes)
        .map_err(proof_to_unauthorized)?;

    // Reserve the tunnel slot. One live tunnel per team. Also record the proof
    // timestamp so a captured proof is single-use: a replay with the same or an
    // older timestamp is rejected here. A legitimate reconnect carries a
    // strictly later timestamp; the one edge case is a sub-second reconnect
    // colliding on the same `unix_secs`, which the herald's 2s backoff rides
    // past (brief offline flap on rapid reconnect).
    let (tx, rx) = broadcast::channel::<HeraldInbound>(crate::state::BROADCAST_CAPACITY);
    let id = Arc::new(());
    {
        let mut reg = state.write()?;
        if reg.presence.contains_key(&team_id) {
            return Err(ApiError::conflict("team already has a live tunnel"));
        }
        // Replay guard: reject a stale/replayed proof, GC out-of-window entries,
        // then record this timestamp. `>=` makes an equal-timestamp proof
        // single-use.
        reg.consume_tunnel_proof(&team_id, ts, state.limits.proof_skew_secs, now)?;
        reg.register_presence(&team_id, tx, id.clone());
    }
    tracing::info!(team = %team_id, "herald tunnel established; team online");

    let lag_team = team_id.clone();
    let stream: BoxEventStream =
        Box::pin(BroadcastStream::new(rx).filter_map(move |r| match r {
            Ok(env) => Some(env),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                // The herald fell behind: forwarded envelopes were dropped. Log
                // so the operator sees the loss; recovery is the herald's
                // reconnect/host-history re-pull.
                tracing::warn!(lag = n, team = %lag_team, "herald tunnel lagged; envelopes dropped");
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
    // still the live one (no reconnect has swapped in a new identity).
    let cleanup_state = state.clone();
    let cleanup_team = team_id.clone();
    let cleanup_id = id.clone();
    let cleaned = OnDrop::new(stream, move || {
        let Ok(mut reg) = cleanup_state.write() else {
            return;
        };
        if reg.take_presence_if_owned(&cleanup_team, &cleanup_id) {
            tracing::info!(team = %cleanup_team, "herald tunnel closed; presence removed");
        }
    });
    Ok(Sse::new(cleaned))
}

/// A rejected tunnel proof is an auth failure.
fn proof_to_unauthorized(e: ProofError) -> ApiError {
    ApiError::unauthorized(format!("invalid device proof: {e}"))
}
