//! Conversation creation + envelope posting (the data plane).
//!
//! `POST /v1/conversations` binds a new direct conversation to `(tagma, agent)`,
//! owned by the caller. `POST /v1/conversations/{id}/envelopes` routes an
//! encrypted envelope to the other endpoint: a user-sent envelope forwards to
//! the tagma's herald tunnel; an agent-sent envelope forwards to the owner's app
//! SSE. The agora validates routing metadata + sender-vs-auth, dedups by
//! `sequence_n`, and never decrypts.
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous, so it does not block the runtime), and the dedup window is a
//! separate `seq_seen` lock. The two locks are never held together. A sequence
//! number is reserved before routing and rolled back if routing fails (e.g. the
//! peer is offline), so a retried envelope with the same `sequence_n` succeeds.

use crate::db::entity::tagmata;
use crate::db::map_db_err;
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
use kallip_common::agentid::AgentId;
use kallip_common::protocol::ApiError;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use crate::auth::{AuthPrincipal, require_tagma, require_user};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
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

#[derive(Deserialize)]
struct CreateConversationRequest {
    tagma_id: String,
    agent_id: AgentId,
}

#[derive(Serialize)]
struct CreateConversationResponse {
    conversation_id: String,
}

async fn create_conversation(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Json(req): Json<CreateConversationRequest>,
) -> Result<Json<CreateConversationResponse>, ApiError> {
    let user = require_user(&principal)?;
    let tagma_id = TagmaId::from(req.tagma_id);
    // Resolve tagma ownership from the durable store BEFORE taking the registry
    // write lock (no `.await` under a guard).
    let tagma = tagmata::Entity::find_by_id(tagma_id.to_string())
        .one(&state.db)
        .await
        .map_err(map_db_err)?;
    // Existence-oracle hardening: a non-owner gets the same 404 as for an
    // unknown tagma, so they cannot confirm whether a guessed tagma id exists.
    let owned = matches!(tagma, Some(t) if t.owner_user_id.as_str() == user.as_ref());
    if !owned {
        return Err(ApiError::not_found("unknown tagma"));
    }
    let mut reg = state.write()?;
    let conv_id = reg.create_conversation(
        user,
        tagma_id,
        req.agent_id,
        state.limits.max_conversations_per_user,
    )?;
    Ok(Json(CreateConversationResponse {
        conversation_id: conv_id.to_string(),
    }))
}

async fn post_envelope(
    State(state): State<SharedState>,
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
    // target - all under a read lock (no mutation here).
    let (sender_key, route) = {
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
                    // Existence-oracle hardening: a non-owner gets the same 404
                    // as for an unknown conversation, so they cannot confirm a
                    // guessed conv id (consistent with create_conversation and
                    // key_exchange_init).
                    return Err(ApiError::not_found("unknown conversation"));
                }
                let route = reg
                    .presence
                    .get(&conv.tagma_id)
                    .map(|p| Route::Herald(p.tx.clone()));
                (format!("user:{user_id}"), route)
            }
            Participant::Agent { tagma_id, agent_id } => {
                let authed_tagma = require_tagma(&principal)?;
                if tagma_id != authed_tagma
                    || agent_id != &conv.agent_id
                    || tagma_id != &conv.tagma_id
                {
                    return Err(ApiError::forbidden(
                        "envelope agent does not match conversation",
                    ));
                }
                let route = reg.app_stream(&conv.owner).cloned().map(Route::App);
                (format!("agent:{tagma_id}:{agent_id}"), route)
            }
        }
    };
    let user_sent = matches!(route, Some(Route::Herald(_)));

    // Reserve the sequence number (atomic check-and-insert). A retried/replayed
    // send (sequence_n <= highest) is rejected.
    {
        let mut seen = state
            .seq_seen
            .lock()
            .map_err(|e| ApiError::internal(format_args!("seq_seen lock poisoned: {e}")))?;
        let conv_seen = seen.entry(conv_id.clone()).or_default();
        if let Some(prev) = conv_seen.get(&sender_key)
            && env.sequence_n <= *prev
        {
            return Err(ApiError::conflict("stale or duplicate sequence_n"));
        }
        conv_seen.insert(sender_key.clone(), env.sequence_n);
    }

    // Route. A `send` failure means no live receiver (peer offline): roll back
    // the reserved sequence number and surface 503 so the sender can retry with
    // the same sequence_n.
    let seq = env.sequence_n;
    let delivered = match route {
        Some(Route::Herald(tx)) => tx.send(HeraldInbound::Envelope { envelope: env }).is_ok(),
        Some(Route::App(tx)) => tx.send(AgoraEvent::Envelope { envelope: env }).is_ok(),
        None => false,
    };
    if !delivered {
        rollback_seq(state.clone(), &conv_id, &sender_key, seq);
        return Err(ApiError::unavailable(if user_sent {
            "tagma is offline"
        } else {
            "user app is offline"
        }));
    }
    Ok(StatusCode::ACCEPTED)
}

/// Remove a reserved sequence number iff it has not since advanced (a later
/// legitimate envelope may have committed a higher number while this one was
/// being routed).
fn rollback_seq(state: SharedState, conv_id: &ConversationId, sender_key: &str, seq: u64) {
    let Ok(mut seen) = state.seq_seen.lock() else {
        return;
    };
    if let Some(conv_seen) = seen.get_mut(conv_id)
        && conv_seen.get(sender_key) == Some(&seq)
    {
        conv_seen.remove(sender_key);
    }
}

/// A resolved route target carrying its typed broadcast sender.
enum Route {
    Herald(tokio::sync::broadcast::Sender<HeraldInbound>),
    App(tokio::sync::broadcast::Sender<AgoraEvent>),
}

/// App -> herald (synchronous): start a conversation key exchange and block
/// until the herald relays its signed response back via
/// [`key_exchange_response`]. The agora forwards the app's ephemeral X25519
/// public key to the tagma's herald over its tunnel, registers a correlated
/// oneshot waiter, and returns the herald's response inline. Fails with 504
/// after `key_exchange_timeout`, or 409 if a key exchange is already in flight
/// for this conversation.
async fn key_exchange_init(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(init): Json<KeyExchangeInit>,
) -> Result<Json<KeyExchangeResponse>, ApiError> {
    let conv_id = ConversationId::from(id);
    let user = require_user(&principal)?;

    // Resolve conversation ownership and the herald tunnel sender under a read
    // lock, then release before any await.
    let (agent_id, sender) = {
        let reg = state.read()?;
        let conv = reg
            .conversations
            .get(&conv_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("unknown conversation"))?;
        if &conv.owner != user {
            // Existence-oracle hardening: non-owner gets the same 404 as unknown.
            return Err(ApiError::not_found("unknown conversation"));
        }
        let sender = reg
            .presence
            .get(&conv.tagma_id)
            .map(|p| p.tx.clone())
            .ok_or_else(|| ApiError::unavailable("tagma is offline"))?;
        (conv.agent_id, sender)
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

    // Best-effort forward: a send failure (herald tunnel just dropped) simply
    // means the await below times out.
    let _ = sender.send(HeraldInbound::KeyExchange {
        conversation_id: conv_id.clone(),
        agent_id,
        init,
    });

    // The guard removes our pending entry on drop unless we disarm it on
    // success, covering the case where the app cancels the request mid-await
    // (axum drops the future, so the match arms below never run).
    let mut guard = KexGuard {
        state: state.clone(),
        conv_id: conv_id.clone(),
        armed: true,
    };
    match tokio::time::timeout(state.limits.key_exchange_timeout, rx).await {
        Ok(Ok(response)) => {
            // The response handler already removed the entry; disarm so a
            // concurrent re-init's fresh entry is not touched on drop.
            guard.armed = false;
            Ok(Json(response))
        }
        Err(_) => Err(ApiError::gateway_timeout("key exchange timed out")),
        // Unreachable under this design: the sender lives in pending_key_exchange
        // until the response handler removes and sends it. Kept defensive.
        Ok(Err(_)) => Err(ApiError::gateway_timeout("key exchange aborted")),
    }
}

/// Herald -> app (resolves a pending [`key_exchange_init`]): the herald's signed
/// key-exchange response. The agora resolves the still-open init request with
/// this body; the app verifies the signature against the pinned key and derives
/// the E2E key. Returns 409 if no init is waiting (stale, or the app already
/// timed out / canceled).
async fn key_exchange_response(
    State(state): State<SharedState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
    Json(response): Json<KeyExchangeResponse>,
) -> Result<StatusCode, ApiError> {
    let conv_id = ConversationId::from(id);
    let tagma = require_tagma(&principal)?;
    // Validate tagma ownership. No TOCTOU: ConversationRecord.tagma_id is
    // immutable post-create and conversations are not deletable.
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
    // Resolve the pending init if any. Ignore a send error: it means the app
    // already gave up (its receiver was dropped).
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
/// Covers app-request cancellation: when axum drops the init handler's future
/// mid-await, the match arms do not run, so without this guard the entry would
/// linger and falsely 409 a subsequent re-init. On the success path the response
/// handler has already removed the entry, so the init handler disarms to avoid
/// racing a concurrent re-init's fresh entry.
struct KexGuard {
    state: SharedState,
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
    //! Handler-level integration tests. Handlers are called directly: their
    //! `State`/`AuthPrincipal`/`Path`/`Json` parameters all have public tuple
    //! fields, so no axum extractor machinery is needed. State is seeded via
    //! [`crate::test_helpers`]. The synchronous KEX correlation is driven by
    //! spawning the blocking init on a task and resolving it (or canceling it)
    //! from the test task.

    use std::time::Duration;

    use axum::Json;
    use axum::extract::{Path, State};
    use kallip_agora_common::bytes::{
        Ciphertext, Ed25519PublicKey, Ed25519Signature, X25519PublicKey,
    };
    use kallip_agora_common::control::{KeyExchangeInit, KeyExchangeResponse};
    use kallip_agora_common::herald::HeraldInbound;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
    use kallip_agora_common::message::{Envelope, Participant};
    use time::OffsetDateTime;

    use super::{key_exchange_init, key_exchange_response, post_envelope};
    use crate::auth::{AuthPrincipal, Principal};
    use crate::test_helpers::{
        make_state, seed_conversation, seed_presence, seed_tagma, seed_user,
    };

    /// A dummy 32-byte X25519 public key for the app's KEX init.
    fn dummy_x25519() -> X25519PublicKey {
        X25519PublicKey(vec![0u8; 32])
    }

    /// A dummy KEX response (the agora relays without verifying, so arbitrary
    /// bytes suffice).
    fn dummy_response() -> KeyExchangeResponse {
        KeyExchangeResponse {
            ephemeral_public: X25519PublicKey(vec![1u8; 32]),
            signature: Ed25519Signature(vec![2u8; 64]),
        }
    }

    /// Seed a full conversation: owner user, online tagma, conversation bound to
    /// a fixed agent, returning the pieces a KEX test needs.
    async fn seed_kex_fixture(
        state: &crate::state::SharedState,
    ) -> (
        UserId,
        TagmaId,
        ConversationId,
        tokio::sync::broadcast::Sender<HeraldInbound>,
    ) {
        let owner = seed_user(state, "owner", "owner@example.test").await;
        let (tagma, _) = seed_tagma(state, &owner, Ed25519PublicKey(vec![0u8; 32])).await;
        let conv = seed_conversation(
            state,
            &owner,
            &tagma,
            kallip_common::agentid::AgentId::from("agent-1".to_string()),
        );
        let tx = seed_presence(state, &tagma);
        (owner, tagma, conv, tx)
    }

    /// Init blocks until the herald's response is relayed back; the response
    /// handler resolves it and the init returns the response body.
    #[tokio::test]
    async fn kex_normal_round_trip() {
        let state = make_state(Duration::from_secs(2)).await;
        let (owner, tagma, conv, tx) = seed_kex_fixture(&state).await;
        let mut rx = tx.subscribe();
        let init = KeyExchangeInit {
            ephemeral_public: dummy_x25519(),
        };

        // Spawn the blocking init. It registers the waiter, pushes the init into
        // the tunnel, and awaits the correlated response.
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

        // The init forwarded the request into the herald tunnel.
        let inbound = rx.recv().await.expect("tunnel message");
        let forwarded_conv = match inbound {
            HeraldInbound::KeyExchange {
                conversation_id, ..
            } => conversation_id,
            other => panic!("expected KeyExchange, got {other:?}"),
        };
        assert_eq!(forwarded_conv, conv);

        // Herald responds; the agora resolves the pending init.
        let expected = dummy_response();
        let resp = key_exchange_response(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(expected.clone()),
        )
        .await
        .expect("response accepted");
        assert_eq!(resp, axum::http::StatusCode::NO_CONTENT);

        // The spawned init unblocks with the relayed response (no PartialEq on
        // the wire type; compare the inner byte vecs).
        let got = handle.await.unwrap().expect("init ok").0;
        assert_eq!(got.ephemeral_public.0, expected.ephemeral_public.0);
        assert_eq!(got.signature.0, expected.signature.0);
    }

    /// A second init while one is already in flight is rejected with 409.
    #[tokio::test]
    async fn kex_duplicate_init_returns_409() {
        let state = make_state(Duration::from_secs(2)).await;
        let (owner, _tagma, conv, tx) = seed_kex_fixture(&state).await;
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
        // Wait for A to register and reach the await.
        let _ = rx.recv().await.expect("tunnel message");

        // A second init on the same conversation hits the in-flight guard.
        let err = key_exchange_init(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(KeyExchangeInit {
                ephemeral_public: X25519PublicKey(vec![3u8; 32]),
            }),
        )
        .await
        .expect_err("duplicate init should 409");
        assert_eq!(err.status, 409);

        handle.abort();
    }

    /// A response with no pending init is rejected with 409.
    #[tokio::test]
    async fn kex_response_without_pending_returns_409() {
        let state = make_state(Duration::from_secs(2)).await;
        let (owner, tagma, conv, _tx) = seed_kex_fixture(&state).await;
        let _ = owner;
        let err = key_exchange_response(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(dummy_response()),
        )
        .await
        .expect_err("no pending init should 409");
        assert_eq!(err.status, 409);
    }

    /// Canceling the init request (axum dropping the future) frees the pending
    /// slot via `KexGuard`, so a subsequent re-init is not falsely rejected.
    /// Dropping a `JoinHandle` only detaches, so we `abort()` then `await` to
    /// deterministically run the future's drop (and the guard).
    #[tokio::test]
    async fn kex_cancel_frees_slot() {
        let state = make_state(Duration::from_secs(5)).await;
        let (owner, tagma, conv, tx) = seed_kex_fixture(&state).await;
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
        // Wait for the init to register and reach the await.
        let _ = rx.recv().await.expect("tunnel message");

        // Cancel (mirrors the app dropping its HTTP request) and wait for the
        // runtime to drop the future, which runs KexGuard::drop.
        handle.abort();
        let _ = handle.await;

        // The pending entry is gone: a re-init would not hit the 409 guard.
        let pending = state.pending_key_exchange.lock().expect("pending lock");
        assert!(
            !pending.contains_key(&conv),
            "KexGuard must remove the entry on cancel"
        );
        let _ = tagma;
    }

    /// If the herald never responds, the init times out with 504 and frees the
    /// slot. >=200ms keeps the test deterministic on loaded runners.
    #[tokio::test]
    async fn kex_timeout_returns_504() {
        let state = make_state(Duration::from_millis(200)).await;
        let (owner, _tagma, conv, _tx) = seed_kex_fixture(&state).await;
        let err = key_exchange_init(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(KeyExchangeInit {
                ephemeral_public: dummy_x25519(),
            }),
        )
        .await
        .expect_err("no herald response should time out");
        assert_eq!(err.status, 504);
        let pending = state.pending_key_exchange.lock().expect("pending lock");
        assert!(!pending.contains_key(&conv), "timeout must free the slot");
    }

    /// A user who does not own the conversation gets 404 (existence-oracle), not
    /// 403, so they cannot confirm a guessed conversation id.
    #[tokio::test]
    async fn post_envelope_non_owner_404() {
        let state = make_state(Duration::from_secs(2)).await;
        let (owner, tagma, conv, _tx) = seed_kex_fixture(&state).await;
        let other = seed_user(&state, "other", "other@example.test").await;
        let _ = owner;
        let env = Envelope {
            conversation_id: conv.clone(),
            sender: Participant::User {
                user_id: other.clone(),
            },
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        };
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::User(other)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("non-owner should be rejected");
        assert_eq!(err.status, 404);
        let _ = tagma;
    }

    /// An envelope whose body `conversation_id` differs from the path is
    /// rejected with 400; the path is authoritative.
    #[tokio::test]
    async fn post_envelope_conversation_id_mismatch_400() {
        let state = make_state(Duration::from_secs(2)).await;
        let (owner, _tagma, conv, _tx) = seed_kex_fixture(&state).await;
        let other_conv = ConversationId::from("other".to_string());
        let env = Envelope {
            conversation_id: other_conv,
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
        .expect_err("mismatched conversation_id should be rejected");
        assert_eq!(err.status, 400);
    }
}
