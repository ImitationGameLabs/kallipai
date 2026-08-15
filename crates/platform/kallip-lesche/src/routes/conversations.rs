//! Conversation resolution + envelope posting.
//!
//! `POST /v1/conversations` resolves (and, on first call, lazily provisions the
//! soft-state record for) the single conversation a tagma owns with its
//! operator. `POST /v1/conversations/{id}/envelopes` routes an encrypted
//! envelope to the other endpoint. The relay validates routing metadata +
//! sender-vs-auth and never decrypts. It is agent-free: an agent sender is
//! attributed only to its tagma.
//!
//! Replay/dedup: NONE at the relay. `sequence_n` is an end-to-end (app<->
//! tagma) counter scoped to a crypto epoch, which the relay cannot see (it has
//! no key), so a relay-side integer window would misalign with the app's
//! per-KEX counter reset and reject a fresh epoch's first message. Replay
//! protection is solely the tagma's job: a per-epoch `seen_inbound` window
//! (within-epoch replay) plus AEAD key rotation (cross-epoch replay).
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous), never co-held with a `ControlPlane` call.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use kallip_agora_common::ids::{ConversationId, ParticipantId, ParticipantKind, TagmaId};
use kallip_agora_common::principal::Principal;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::control::{KeyExchangeInit, KeyExchangeResponse};
use kallip_lesche_common::event::LescheEvent;
use kallip_lesche_common::message::Envelope;
use kallip_lesche_common::tunnel::TunnelInbound;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthPrincipal, require_tagma, require_user};
use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new()
        .route("/conversations", post(create_conversation))
        .route("/conversations/{id}/envelopes", post(post_envelope))
        .route(
            "/conversations/{id}/key-exchange/init",
            post(key_exchange_init),
        )
        .route(
            "/conversations/{id}/key-exchange/response",
            post(key_exchange_response),
        )
}

/// `POST /v1/conversations { tagma_id }` - resolve the single conversation this
/// tagma owns with its operator. The tagma must be enrolled and owned by the
/// caller (existence-oracle 404 otherwise). Idempotent: the conversation id is
/// the deterministic `ConversationId::for_tagma` derivation.
#[derive(Deserialize)]
struct CreateConversationRequest {
    tagma_id: String,
}

#[derive(Serialize, Debug)]
struct CreateConversationResponse {
    conversation_id: String,
}

async fn create_conversation(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateConversationRequest>,
) -> Result<Json<CreateConversationResponse>, ApiError> {
    let user = require_user(&principal)?.clone();
    let tagma_id = TagmaId::from(req.tagma_id);
    // Resolve tagma ownership from the registry's raw facts BEFORE taking the
    // registry write lock (no `.await` under a guard). The predicate (enrolled +
    // owned by caller) is derived locally; any failure -- unknown / pending /
    // non-owner -- collapses to one "unknown tagma" 404 (the existence-oracle).
    let resolvable = crate::control_policy::tagma_profile(&*state.control, &tagma_id)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?
        .as_ref()
        .is_some_and(|p| crate::control_policy::bilateral_resolvable(p, &user));
    if !resolvable {
        return Err(ApiError::not_found("unknown tagma"));
    }
    let mut reg = state.write()?;
    let conv_id = reg.ensure_conversation(&user, &tagma_id);
    Ok(Json(CreateConversationResponse {
        conversation_id: conv_id.to_string(),
    }))
}

async fn post_envelope(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(env): Json<Envelope>,
) -> Result<StatusCode, ApiError> {
    let conv_id = ConversationId::from(id);

    // The path is authoritative: a body claiming a different conversation_id
    // would otherwise be trusted by the tagma, which keys its decrypt state on
    // the envelope field.
    if env.conversation_id != conv_id {
        return Err(ApiError::bad_request(
            "envelope conversation_id does not match the path",
        ));
    }

    // Resolve the conversation, validate the poster vs the conversation, and
    // capture the route target - all under a read lock (no mutation, no await).
    //
    // Route by WHO is posting (the principal), NOT by `env.sender.kind`: the
    // tagma legitimately re-attributes the owner's rows to the Human sender (the
    // live `UserMessage` echo and the history-batch replay of inbound rows), so
    // `sender.kind` is *attribution*, not direction. Routing by sender.kind made
    // those tagma-posted, Human-attributed envelopes fail `require_user` (403),
    // dropping the echo and the history replay so a sent message vanished after
    // refresh. The principal alone encodes direction: a user posts toward the
    // tagma, a tagma posts toward the app.
    let route = {
        let reg = state.read()?;
        let conv = reg
            .conversations
            .get(&conv_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("unknown conversation"))?;
        match &principal {
            Principal::User(user) => {
                // app -> tagma: only the owner, posting a Human-sender envelope
                // attributed to themselves.
                if user != &conv.owner {
                    return Err(ApiError::not_found("unknown conversation"));
                }
                if env.sender.kind != ParticipantKind::Human
                    || env.sender.id != ParticipantId::for_user(user)
                {
                    return Err(ApiError::forbidden("envelope sender does not match auth"));
                }
                reg.presence_by_tagma(&conv.tagma_id)
                    .map(|p| Route::Tagma(p.tx.clone()))
            }
            Principal::Tagma(tagma) => {
                // tagma -> app: a non-owner tagma is a non-participant (collapse
                // to the existence-oracle 404, matching the user arm + rooms).
                if tagma != &conv.tagma_id {
                    return Err(ApiError::not_found("unknown conversation"));
                }
                // The conversation's tagma is the trusted principal for this
                // conversation; it echoes/replays rows it owns, attributing the
                // sender itself (its own Agent voice, or a Human echo of the
                // owner's inbound row). The relay deliberately does NOT fence on
                // `sender.id`: historical rows may carry legacy/derived ids the
                // tagma cannot re-derive (it does not know the owner's raw
                // user_id), and fencing on it aborted history replay mid-batch
                // (one stale row 403'd the whole batch). This is safe because
                // the route target is owner-derived (`app_stream(&conv.owner)`)
                // -- a bilateral conversation is strictly owner<->tagma, so the
                // stamped sender can only affect rendering on the owner's own
                // stream, never reach a third party. Ownership is the gate.
                reg.app_stream(&conv.owner).cloned().map(Route::App)
            }
            Principal::Admin => {
                // An admin is not a conversation participant.
                return Err(ApiError::not_found("unknown conversation"));
            }
        }
    };
    let user_sent = matches!(route, Some(Route::Tagma(_)));

    // Route. No relay-side replay/dedup window: `sequence_n` is an end-to-end
    // (app<->tagma) counter scoped to a crypto epoch, and the relay cannot see
    // the epoch (no key). Replay protection is solely the tagma's job -- a
    // per-epoch window (`seen_inbound`) for within-epoch replay, plus AEAD
    // key rotation for cross-epoch replay. A relay-side integer window would
    // misalign with the app's per-KEX counter reset (rejecting a fresh epoch's
    // first message). A `send` failure here means no live receiver (peer
    // offline); surface 503 so the sender can retry.
    let delivered = match route {
        Some(Route::Tagma(tx)) => tx.send(TunnelInbound::Envelope { envelope: env }).is_ok(),
        Some(Route::App(tx)) => tx.send(LescheEvent::Envelope { envelope: env }).is_ok(),
        None => false,
    };
    if !delivered {
        return Err(ApiError::unavailable(if user_sent {
            "tagma is offline"
        } else {
            "user app is offline"
        }));
    }
    Ok(StatusCode::ACCEPTED)
}

/// A resolved route target carrying its typed broadcast sender.
enum Route {
    Tagma(tokio::sync::broadcast::Sender<TunnelInbound>),
    App(tokio::sync::broadcast::Sender<LescheEvent>),
}

/// App -> tagma (synchronous): start a conversation key exchange and block
/// until the tagma relays its signed response back. Fails with 504 after
/// `key_exchange_timeout`, or 409 if a KEX is already in flight.
async fn key_exchange_init(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(init): Json<KeyExchangeInit>,
) -> Result<Json<KeyExchangeResponse>, ApiError> {
    let conv_id = ConversationId::from(id);
    let user = require_user(&principal)?;

    // Resolve conversation ownership and the tunnel sender under a read
    // lock, then release before any await.
    let sender = {
        let reg = state.read()?;
        let conv = reg
            .conversations
            .get(&conv_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("unknown conversation"))?;
        if &conv.owner != user {
            return Err(ApiError::not_found("unknown conversation"));
        }
        reg.presence_by_tagma(&conv.tagma_id)
            .map(|p| p.tx.clone())
            .ok_or_else(|| ApiError::unavailable("tagma is offline"))?
    };

    // Register the waiter BEFORE pushing the init: the tagma's response may
    // arrive almost immediately and must find a pending entry.
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut pending = state.pending_key_exchange.lock().map_err(|e| {
            ApiError::internal(format_args!("pending_key_exchange lock poisoned: {e}"))
        })?;
        if pending.contains_key(&conv_id) {
            return Err(ApiError::conflict(
                "key exchange already in progress for conversation",
            ));
        }
        pending.insert(conv_id.clone(), tx);
    }

    let _ = sender.send(TunnelInbound::KeyExchange {
        conversation_id: conv_id.clone(),
        init,
    });

    let mut guard = KexGuard {
        state: state.clone(),
        conv_id: conv_id.clone(),
        armed: true,
    };
    match tokio::time::timeout(state.key_exchange_timeout, rx).await {
        Ok(Ok(response)) => {
            guard.armed = false;
            Ok(Json(response))
        }
        Err(_) => Err(ApiError::gateway_timeout("key exchange timed out")),
        Ok(Err(_)) => Err(ApiError::gateway_timeout("key exchange aborted")),
    }
}

/// Tagma -> app (resolves a pending [`key_exchange_init`]). Returns 409 if no
/// init is waiting.
async fn key_exchange_response(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(response): Json<KeyExchangeResponse>,
) -> Result<StatusCode, ApiError> {
    let conv_id = ConversationId::from(id);
    let tagma = require_tagma(&principal)?;
    {
        let reg = state.read()?;
        let conv = reg
            .conversations
            .get(&conv_id)
            .ok_or_else(|| ApiError::not_found("unknown conversation"))?;
        if &conv.tagma_id != tagma {
            return Err(ApiError::forbidden("not the conversation's tagma"));
        }
    }
    let tx = {
        let mut pending = state.pending_key_exchange.lock().map_err(|e| {
            ApiError::internal(format_args!("pending_key_exchange lock poisoned: {e}"))
        })?;
        pending.remove(&conv_id)
    };
    match tx {
        Some(tx) => {
            let _ = tx.send(response);
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(ApiError::conflict(
            "no pending key exchange for conversation",
        )),
    }
}

/// Removes a registered `pending_key_exchange` entry on drop, unless disarmed.
struct KexGuard {
    state: SharedConvState,
    conv_id: ConversationId,
    armed: bool,
}

impl Drop for KexGuard {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut pending) = self.state.pending_key_exchange.lock()
        {
            pending.remove(&self.conv_id);
        }
    }
}

#[cfg(test)]
mod tests;
