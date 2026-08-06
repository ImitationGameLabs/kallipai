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
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_agora_common::bytes::{Ciphertext, Ed25519PublicKey, X25519PublicKey};
    use kallip_agora_common::ids::{
        ConversationId, ParticipantId, ParticipantKind, TagmaId, TraceId, UserId,
    };
    use kallip_agora_common::principal::Principal;
    use kallip_lesche_common::control::KeyExchangeInit;
    use kallip_lesche_common::message::Participant;
    use kallip_lesche_common::tunnel::TunnelInbound;
    use time::OffsetDateTime;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    fn dummy_x25519() -> X25519PublicKey {
        X25519PublicKey(vec![0u8; 32])
    }

    fn dummy_response() -> kallip_lesche_common::control::KeyExchangeResponse {
        kallip_lesche_common::control::KeyExchangeResponse {
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
        tokio::sync::broadcast::Sender<TunnelInbound>,
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

    /// Parity with the old prod `tagma_resolvable_by`, which did NOT check
    /// `revoked` (only owner + enrolled): the owner may still open a bilateral
    /// conversation with their own enrolled-but-revoked tagma. The old MOCK was
    /// stricter (it checked revoked); this test locks the prod-faithful behavior
    /// now that the predicate lives in the relay.
    #[tokio::test]
    async fn create_conversation_succeeds_for_enrolled_owned_revoked_tagma() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        control.revoke_tagma(&tagma);
        let Json(resp) = create_conversation(
            State(state),
            AuthPrincipal(Principal::User(owner)),
            Json(CreateConversationRequest {
                tagma_id: tagma.to_string(),
            }),
        )
        .await
        .expect("owner may open a bilateral chat with their own enrolled tagma");
        assert!(!resp.conversation_id.is_empty());
    }

    /// Existence-oracle preservation at the bilateral-create gate: an unknown
    /// tagma and an enrolled-but-not-owned tagma produce the byte-identical 404
    /// body (no leak of *why*).
    #[tokio::test]
    async fn create_conversation_oracle_body_is_uniform_across_failure_modes() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let other = user("other");
        let owned = TagmaId::from("owned-1".to_string());
        control.enroll_tagma(&owned, owner, Ed25519PublicKey(vec![0u8; 32]), "tok");

        // Unknown tagma (not seeded) vs. enrolled-but-not-owned (other is not the
        // owner) -- both collapse to the same "unknown tagma" 404.
        let e_unknown = create_conversation(
            State(state.clone()),
            AuthPrincipal(Principal::User(other.clone())),
            Json(CreateConversationRequest {
                tagma_id: "ghost".to_string(),
            }),
        )
        .await
        .expect_err("unknown tagma");
        let e_non_owner = create_conversation(
            State(state),
            AuthPrincipal(Principal::User(other)),
            Json(CreateConversationRequest {
                tagma_id: owned.to_string(),
            }),
        )
        .await
        .expect_err("non-owner 404");
        assert_eq!(e_unknown.status, 404);
        assert_eq!(e_unknown.message, e_non_owner.message);
    }

    #[tokio::test]
    async fn post_envelope_conversation_id_mismatch_400() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let env = Envelope {
            conversation_id: ConversationId::from("other".to_string()),
            sender: Participant {
                id: ParticipantId::for_user(&owner),
                kind: ParticipantKind::Human,
                handle: "Owner".into(),
                tagma_id: None,
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

    /// Build a minimal envelope with the given sender attributed to `conv`.
    /// Ciphertext/seq are irrelevant to routing (the relay never decrypts).
    fn envelope(conv: &ConversationId, sender: Participant) -> Envelope {
        Envelope {
            conversation_id: conv.clone(),
            sender,
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        }
    }

    fn human(id: ParticipantId) -> Participant {
        Participant {
            id,
            kind: ParticipantKind::Human,
            handle: "h".into(),
            tagma_id: None,
        }
    }

    fn agent(id: ParticipantId) -> Participant {
        Participant {
            id,
            kind: ParticipantKind::Agent,
            handle: "a".into(),
            tagma_id: None,
        }
    }

    /// app -> tagma: the owner posts their own Human envelope, and it lands on
    /// the tagma's tunnel.
    #[tokio::test]
    async fn post_envelope_user_to_tagma_accepted() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, tx) = seed_fixture(&state, &control, &owner);
        let mut rx = tx.subscribe();
        let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
        let status = post_envelope(
            State(state),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect("user->tagma accepted");
        assert_eq!(status, StatusCode::ACCEPTED);
        match rx.recv().await.expect("tunnel delivery") {
            TunnelInbound::Envelope { .. } => {}
            other => panic!("expected Envelope on tunnel, got {other:?}"),
        }
    }

    /// A non-owner user posting to the owner's conversation is a non-participant:
    /// 404 (existence-oracle), not 403.
    #[tokio::test]
    async fn post_envelope_user_non_owner_404() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let other = user("other");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let env = envelope(&conv, human(ParticipantId::for_user(&other)));
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::User(other)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("non-owner 404");
        assert_eq!(err.status, 404);
    }

    /// A user may not post an Agent-sender envelope (the kind conjunct).
    #[tokio::test]
    async fn post_envelope_user_agent_sender_403() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let env = envelope(
            &conv,
            agent(ParticipantId::for_tagma(&TagmaId::from("x".to_string()))),
        );
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::User(owner)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("agent sender from user 403");
        assert_eq!(err.status, 403);
    }

    /// THE BUG: the tagma echoes/replays an owner row as a Human-sender
    /// envelope. The relay must accept it (was 403) and deliver it to the
    /// owner's app stream.
    #[tokio::test]
    async fn post_envelope_tagma_echoes_owner_row_accepted() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let app_tx = state.write().unwrap().open_app_stream(&owner);
        let mut app_rx = app_tx.subscribe();
        let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
        let status = post_envelope(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect("tagma echo accepted");
        assert_eq!(status, StatusCode::ACCEPTED);
        match app_rx.recv().await.expect("app stream delivery") {
            LescheEvent::Envelope { .. } => {}
            other => panic!("expected Envelope on app stream, got {other:?}"),
        }
    }

    /// The tagma's own Agent reply routes to the owner's app stream.
    #[tokio::test]
    async fn post_envelope_tagma_agent_reply_accepted() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let app_tx = state.write().unwrap().open_app_stream(&owner);
        let mut app_rx = app_tx.subscribe();
        let env = envelope(&conv, agent(ParticipantId::for_tagma(&tagma)));
        let status = post_envelope(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect("agent reply accepted");
        assert_eq!(status, StatusCode::ACCEPTED);
        match app_rx.recv().await.expect("app stream delivery") {
            LescheEvent::Envelope { .. } => {}
            other => panic!("expected Envelope on app stream, got {other:?}"),
        }
    }

    /// The tagma arm does NOT fence on `sender.id`: history replay must tolerate
    /// legacy/derived ids the tagma cannot re-derive (it does not know the
    /// owner's raw user_id). A tagma-posted envelope with a non-derived sender id
    /// is accepted and routed to the owner's app stream (ownership is the gate;
    /// the route target is owner-derived).
    #[tokio::test]
    async fn post_envelope_tagma_legacy_sender_id_accepted() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let app_tx = state.write().unwrap().open_app_stream(&owner);
        let mut app_rx = app_tx.subscribe();
        // A Human-sender id that is NOT the v5 derivation of the owner (e.g. a
        // legacy raw id stored in chat_history). Accepted all the same.
        let env = envelope(
            &conv,
            human(ParticipantId::from("legacy-user-id".to_string())),
        );
        let status = post_envelope(
            State(state),
            AuthPrincipal(Principal::Tagma(tagma)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect("legacy sender id accepted");
        assert_eq!(status, StatusCode::ACCEPTED);
        match app_rx.recv().await.expect("app stream delivery") {
            LescheEvent::Envelope { .. } => {}
            other => panic!("expected Envelope on app stream, got {other:?}"),
        }
    }

    /// A different tagma (not the conversation's tagma) posting, even with its
    /// own legitimate Agent sender, is a non-participant: 404 (existence-oracle).
    #[tokio::test]
    async fn post_envelope_wrong_tagma_404() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let intruder = TagmaId::from("tagma-intruder".to_string());
        control.enroll_tagma(
            &intruder,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let env = envelope(&conv, agent(ParticipantId::for_tagma(&intruder)));
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::Tagma(intruder)),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("wrong tagma 404");
        assert_eq!(err.status, 404);
    }

    /// An admin is not a conversation participant.
    #[tokio::test]
    async fn post_envelope_admin_404() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let (_tagma, conv, _tx) = seed_fixture(&state, &control, &owner);
        let env = envelope(&conv, human(ParticipantId::for_user(&owner)));
        let err = post_envelope(
            State(state),
            AuthPrincipal(Principal::Admin),
            Path(conv.to_string()),
            Json(env),
        )
        .await
        .expect_err("admin 404");
        assert_eq!(err.status, 404);
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
            TunnelInbound::KeyExchange {
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
