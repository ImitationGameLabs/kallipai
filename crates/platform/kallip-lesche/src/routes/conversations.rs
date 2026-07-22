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
//! herald) counter scoped to a crypto epoch, which the relay cannot see (it has
//! no key), so a relay-side integer window would misalign with the app's
//! per-KEX counter reset and reject a fresh epoch's first message. Replay
//! protection is solely the herald's job: a per-epoch `seen_inbound` window
//! (within-epoch replay) plus AEAD key rotation (cross-epoch replay).
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous), never co-held with a `ControlPlane` call.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use kallip_agora_common::control::{KeyExchangeInit, KeyExchangeResponse};
use kallip_agora_common::event::AgoraEvent;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId};
use kallip_agora_common::message::{Envelope, Participant};
use kallip_common::protocol::ApiError;
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
    // Resolve tagma ownership via the registry BEFORE taking the registry write
    // lock (no `.await` under a guard). A single boolean preserves the
    // existence-oracle: unknown / pending / non-owner all 404 the same way.
    let resolvable = state
        .control
        .tagma_resolvable_by(&tagma_id, &user)
        .await
        .map_err(|e| ApiError::internal(format_args!("registry error: {e}")))?;
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
    // would otherwise be trusted by the herald, which keys its decrypt state on
    // the envelope field.
    if env.conversation_id != conv_id {
        return Err(ApiError::bad_request(
            "envelope conversation_id does not match the path",
        ));
    }

    // Resolve the conversation, validate sender-vs-auth, and capture the route
    // target - all under a read lock (no mutation here, no await).
    let route = {
        let reg = state.read()?;
        let conv = reg
            .conversations
            .get(&conv_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("unknown conversation"))?;
        match &env.sender {
            Participant::User { user_id } => {
                let authed = require_user(&principal)?;
                if user_id != authed {
                    return Err(ApiError::forbidden("envelope sender does not match auth"));
                }
                if &conv.owner != authed {
                    return Err(ApiError::not_found("unknown conversation"));
                }
                reg.presence
                    .get(&conv.tagma_id)
                    .map(|p| Route::Herald(p.tx.clone()))
            }
            Participant::Agent { tagma_id } => {
                let authed_tagma = require_tagma(&principal)?;
                if tagma_id != authed_tagma || tagma_id != &conv.tagma_id {
                    return Err(ApiError::forbidden(
                        "envelope agent does not match conversation",
                    ));
                }
                reg.app_stream(&conv.owner).cloned().map(Route::App)
            }
        }
    };
    let user_sent = matches!(route, Some(Route::Herald(_)));

    // Route. No relay-side replay/dedup window: `sequence_n` is an end-to-end
    // (app<->herald) counter scoped to a crypto epoch, and the relay cannot see
    // the epoch (no key). Replay protection is solely the herald's job -- a
    // per-epoch window (`seen_inbound`) for within-epoch replay, plus AEAD
    // key rotation for cross-epoch replay. A relay-side integer window would
    // misalign with the app's per-KEX counter reset (rejecting a fresh epoch's
    // first message). A `send` failure here means no live receiver (peer
    // offline); surface 503 so the sender can retry.
    let delivered = match route {
        Some(Route::Herald(tx)) => tx.send(HeraldInbound::Envelope { envelope: env }).is_ok(),
        Some(Route::App(tx)) => tx.send(AgoraEvent::Envelope { envelope: env }).is_ok(),
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
    Herald(tokio::sync::broadcast::Sender<HeraldInbound>),
    App(tokio::sync::broadcast::Sender<AgoraEvent>),
}

/// App -> herald (synchronous): start a conversation key exchange and block
/// until the herald relays its signed response back. Fails with 504 after
/// `key_exchange_timeout`, or 409 if a KEX is already in flight.
async fn key_exchange_init(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(init): Json<KeyExchangeInit>,
) -> Result<Json<KeyExchangeResponse>, ApiError> {
    let conv_id = ConversationId::from(id);
    let user = require_user(&principal)?;

    // Resolve conversation ownership and the herald tunnel sender under a read
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
        reg.presence
            .get(&conv.tagma_id)
            .map(|p| p.tx.clone())
            .ok_or_else(|| ApiError::unavailable("tagma is offline"))?
    };

    // Register the waiter BEFORE pushing the init: the herald's response may
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

    let _ = sender.send(HeraldInbound::KeyExchange {
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

/// Herald -> app (resolves a pending [`key_exchange_init`]). Returns 409 if no
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
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_agora_common::bytes::{Ciphertext, Ed25519PublicKey, X25519PublicKey};
    use kallip_agora_common::control::KeyExchangeInit;
    use kallip_agora_common::herald::HeraldInbound;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
    use kallip_agora_common::principal::Principal;
    use time::OffsetDateTime;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    fn dummy_x25519() -> X25519PublicKey {
        X25519PublicKey(vec![0u8; 32])
    }

    fn dummy_response() -> kallip_agora_common::control::KeyExchangeResponse {
        kallip_agora_common::control::KeyExchangeResponse {
            ephemeral_public: X25519PublicKey(vec![1u8; 32]),
            signature: kallip_agora_common::bytes::Ed25519Signature(vec![2u8; 64]),
        }
    }

    /// Seed an enrolled tagma + presence, return the pieces KEX tests need.
    fn seed_fixture(
        state: &SharedConvState,
        control: &crate::test_support::MockControlPlane,
        owner: &UserId,
    ) -> (
        TagmaId,
        ConversationId,
        tokio::sync::broadcast::Sender<HeraldInbound>,
    ) {
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        // Provision the conversation record (as a live create_conversation would).
        let conv = {
            let mut reg = state.write().unwrap();
            reg.ensure_conversation(owner, &tagma)
        };
        let (tx, _id) = seed_presence(state, &tagma, owner.clone());
        (tagma, conv, tx)
    }

    #[tokio::test]
    async fn create_conversation_resolves_and_is_idempotent() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let expected = ConversationId::for_tagma(&tagma).to_string();

        let Json(resp) = create_conversation(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner.clone())),
            Json(CreateConversationRequest {
                tagma_id: tagma.to_string(),
            }),
        )
        .await
        .expect("resolve");
        assert_eq!(resp.conversation_id, expected);

        // Idempotent: a second resolve returns the same id and leaves the record.
        let Json(resp2) = create_conversation(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner)),
            Json(CreateConversationRequest {
                tagma_id: tagma.to_string(),
            }),
        )
        .await
        .expect("repeat resolve");
        assert_eq!(resp2.conversation_id, expected);
    }

    #[tokio::test]
    async fn create_conversation_non_owner_404() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let other = user("other");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(&tagma, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");
        let err = create_conversation(
            State(state),
            AuthPrincipal(Principal::User(other)),
            Json(CreateConversationRequest {
                tagma_id: tagma.to_string(),
            }),
        )
        .await
        .expect_err("non-owner 404");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn post_envelope_conversation_id_mismatch_400() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let env = Envelope {
            conversation_id: ConversationId::from("other".to_string()),
            sender: Participant::User {
                user_id: owner.clone(),
            },
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        };
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("mismatch 400");
        assert_eq!(err.status, 400);
    }

    #[tokio::test]
    async fn kex_normal_round_trip() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (tagma, conv, tx) = seed_fixture(&state, &control, &owner);
        let mut rx = tx.subscribe();
        let init = KeyExchangeInit {
            ephemeral_public: dummy_x25519(),
        };

        let state_for_init = state.clone();
        let owner_for_init = owner.clone();
        let conv_for_init = conv.clone();
        let handle = tokio::spawn(async move {
            key_exchange_init(
                State(state_for_init),
                AuthPrincipal(Principal::User(owner_for_init)),
                Path(conv_for_init.to_string()),
                Json(init),
            )
            .await
        });

        let inbound = rx.recv().await.expect("tunnel message");
        let forwarded_conv = match inbound {
            HeraldInbound::KeyExchange {
                conversation_id, ..
            } => conversation_id,
            other => panic!("expected KeyExchange, got {other:?}"),
        };
        assert_eq!(forwarded_conv, conv);

        let expected = dummy_response();
        let resp = key_exchange_response(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(expected.clone()),
        )
        .await
        .expect("response accepted");
        assert_eq!(resp, StatusCode::NO_CONTENT);

        let got = handle.await.unwrap().expect("init ok").0;
        assert_eq!(got.ephemeral_public.0, expected.ephemeral_public.0);
    }

    #[tokio::test]
    async fn kex_duplicate_init_returns_409() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, tx) = seed_fixture(&state, &control, &owner);
        let mut rx = tx.subscribe();

        let state_for_init = state.clone();
        let owner_for_init = owner.clone();
        let conv_for_init = conv.clone();
        let handle = tokio::spawn(async move {
            key_exchange_init(
                State(state_for_init),
                AuthPrincipal(Principal::User(owner_for_init)),
                Path(conv_for_init.to_string()),
                Json(KeyExchangeInit {
                    ephemeral_public: dummy_x25519(),
                }),
            )
            .await
        });
        let _ = rx.recv().await;

        let err = key_exchange_init(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(KeyExchangeInit {
                ephemeral_public: X25519PublicKey(vec![3u8; 32]),
            }),
        )
        .await
        .expect_err("dup 409");
        assert_eq!(err.status, 409);
        handle.abort();
    }

    #[tokio::test]
    async fn kex_timeout_returns_504() {
        let (state, control) = make_state(60, std::time::Duration::from_millis(200));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let err = key_exchange_init(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(KeyExchangeInit {
                ephemeral_public: dummy_x25519(),
            }),
        )
        .await
        .expect_err("timeout 504");
        assert_eq!(err.status, 504);
        assert!(
            !state
                .pending_key_exchange
                .lock()
                .unwrap()
                .contains_key(&conv),
            "timeout must free the slot"
        );
    }

    #[tokio::test]
    async fn kex_response_without_pending_returns_409() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let err = key_exchange_response(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(dummy_response()),
        )
        .await
        .expect_err("no pending 409");
        assert_eq!(err.status, 409);
    }
}
