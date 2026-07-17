//! The herald tunnel: a long-lived SSE the herald holds open to receive
//! forwarded envelopes. Establishing it (with a fresh signed proof of the pinned
//! device key) marks the tagma online; disconnect removes presence only if this
//! tunnel is still the live one. A second concurrent tunnel for one tagma is
//! rejected.
//!
//! Every (re)connect must present `X-Device-Timestamp` + `X-Device-Proof`: an
//! Ed25519 signature over the tunnel transcript, verified against the tagma's
//! pinned key, with the timestamp within `+/- proof_skew_secs`. So a stolen
//! long-lived tagma token alone cannot open a tunnel.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::entity::tagmata;
use crate::db::map_db_err;
use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::proof::{ProofError, verify_tunnel_proof};
use kallip_common::protocol::ApiError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_tagma};
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
    let tagma_id = require_tagma(&principal)?.clone();

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
    // The pinned device key now lives in the durable store (a registered tagma
    // must survive an agora restart). Fetched outside the registry lock.
    let pinned = {
        let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
            .one(&state.db)
            .await
            .map_err(map_db_err)?;
        let tagma = tagma.ok_or_else(|| ApiError::not_found("unknown tagma"))?;
        tagma.pinned_public_key
    };
    verify_tunnel_proof(&pinned, tagma_id.as_ref(), ts, &sig_bytes)
        .map_err(proof_to_unauthorized)?;

    // Durable replay guard: accept this proof only if the tagma's stored
    // high-water-mark timestamp is NULL or strictly less than `ts`. The
    // conditional UPDATE is atomic, so a replay with the same or older
    // timestamp is rejected even after an agora restart (the guard is no longer
    // in-memory). A legitimate reconnect carries a strictly later timestamp;
    // the one edge case is a sub-second reconnect colliding on the same
    // `unix_secs`, which the herald's 2s backoff rides past. Run outside the
    // registry lock (DB access never happens under it).
    let updated = tagmata::Entity::update_many()
        .filter(tagmata::Column::Id.eq(tagma_id.to_string()))
        .filter(
            sea_orm::Condition::any()
                .add(tagmata::Column::LastTunnelProofTs.is_null())
                .add(tagmata::Column::LastTunnelProofTs.lt(ts)),
        )
        .col_expr(
            tagmata::Column::LastTunnelProofTs,
            sea_orm::sea_query::Expr::value(ts),
        )
        .exec(&state.db)
        .await
        .map_err(map_db_err)?;
    if updated.rows_affected == 0 {
        // The stored ts is `>= ts` (replay), or the tagma row vanished between
        // the find above and here. Tagmata are not deletable today, so this is
        // a documented rare-race conflation.
        return Err(ApiError::unauthorized("replayed or stale device proof"));
    }

    // Reserve the tunnel slot. One live tunnel per tagma. (The proof `ts` is
    // already durably recorded; a connection that then loses the presence race
    // has burned its `ts`, which is fine: the herald retries with a fresh,
    // strictly-later `ts`.)
    let (tx, rx) = broadcast::channel::<HeraldInbound>(crate::state::BROADCAST_CAPACITY);
    let id = Arc::new(());
    {
        let mut reg = state.write()?;
        if reg.presence.contains_key(&tagma_id) {
            return Err(ApiError::conflict("tagma already has a live tunnel"));
        }
        reg.register_presence(&tagma_id, tx, id.clone());
    }
    tracing::info!(tagma = %tagma_id, "herald tunnel established; tagma online");

    let lag_tagma = tagma_id.clone();
    let stream: BoxEventStream =
        Box::pin(BroadcastStream::new(rx).filter_map(move |r| match r {
            Ok(env) => Some(env),
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                // The herald fell behind: forwarded envelopes were dropped. Log
                // so the operator sees the loss; recovery is the herald's
                // reconnect/host-history re-pull.
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
    // still the live one (no reconnect has swapped in a new identity).
    let cleanup_state = state.clone();
    let cleanup_tagma = tagma_id.clone();
    let cleanup_id = id.clone();
    let cleaned = OnDrop::new(stream, move || {
        let Ok(mut reg) = cleanup_state.write() else {
            return;
        };
        if reg.take_presence_if_owned(&cleanup_tagma, &cleanup_id) {
            tracing::info!(tagma = %cleanup_tagma, "herald tunnel closed; presence removed");
        }
    });
    Ok(Sse::new(cleaned))
}

/// A rejected tunnel proof is an auth failure.
fn proof_to_unauthorized(e: ProofError) -> ApiError {
    ApiError::unauthorized(format!("invalid device proof: {e}"))
}
