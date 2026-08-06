//! The room-keyed envelope surface: `POST /v1/rooms/{room_id}/envelopes`.
//!
//! An envelope addressed to a room is:
//!
//! 1. **Authorized** -- the sender (a `Participant`) must be the authenticated
//!    principal AND a current member of the room (served from the membership
//!    cache). A non-member or unknown room is a uniform 404.
//! 2. **Persisted** -- the payload is appended to the durable `room_messages`
//!    store so offline members pull it on reconnect (skipped when the relay runs
//!    without a store, i.e. mock tests). Room payloads are plaintext
//!    `RoomMessage` JSON, stored opaquely -- the lesche is the room's store of
//!    record and the server is trusted to read room content.
//! 3. **Fanned** -- delivered to every other member via [`crate::fan::fan_envelope`]
//!    (user members' app streams, tagma members' tunnels).

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{MemberId, ParticipantKind, RoomId};
use kallip_agora_common::principal::Principal;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::message::{Envelope, Participant};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::auth::{AuthPrincipal, OptUserDisplay};
use crate::db::map_db_err;
use crate::db::store;
use crate::fan::fan_envelope;
use crate::identity::{agent_handle, degraded_handle, human_handle};
use crate::member_identity::{MemberRef, degraded, resolve_handles};
use crate::state::{AgentProfile, SharedConvState};

/// Is `(mid, kind)` a current member of `membership`? Both fields are matched:
/// the v5 user/tagma id namespaces are disjoint today, but the kind check is
/// cheap defense-in-depth against any future id-collision and keeps the single
/// AuthZ gate uniform across callers.
fn is_member(
    membership: &kallip_agora_common::control_plane::RoomMembership,
    mid: &MemberId,
    kind: ParticipantKind,
) -> bool {
    membership
        .members
        .iter()
        .any(|m| m.id == *mid && m.kind == kind)
}

pub fn router() -> Router<SharedConvState> {
    Router::new()
        .route("/rooms/{room_id}/envelopes", post(post_room_envelope))
        .route("/rooms/{room_id}/messages", get(room_history))
}

async fn post_room_envelope(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    OptUserDisplay(user_display): OptUserDisplay,
    Path(room_id): Path<String>,
    Json(mut env): Json<Envelope>,
) -> Result<StatusCode, ApiError> {
    let room = RoomId::from(room_id);
    // The path is authoritative: a body claiming a different room would
    // otherwise be trusted downstream.
    if env.conversation_id.as_ref() != room.as_ref() {
        return Err(ApiError::bad_request(
            "envelope conversation_id does not match the path",
        ));
    }

    // Membership is read from the lesche-local graph (one SQL read, strongly
    // consistent with mutations in this DB). Unknown room -> 404.
    let db = state.require_db()?;
    let membership = store::room_membership(db, room.as_ref())
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown room"))?;

    // Authorize: the sender must be the authed principal (id + kind) AND a room
    // member. Existence-oracle: a non-member (and an Admin, who is never a room
    // participant) gets the same 404 as an unknown room. The membership gate runs
    // BEFORE the sender-match, so a spoofed sender cannot be used to distinguish
    // a real from a nonexistent room (a non-member probing with another member's
    // id gets 404, not a 403 that confirms the room exists). The room surface
    // deals only in participant ids, so the underlying user/tagma id never
    // appears here.
    let (authed_pid, authed_kind) = match (principal.participant_id(), principal.participant_kind())
    {
        (Some(pid), Some(kind)) => (pid, kind),
        _ => return Err(ApiError::not_found("unknown room")), // Admin
    };
    // `authed_pid` is the wire `ParticipantId` (used below for the profile cache
    // + handle stamping); the membership gate works in the room-domain twin.
    let authed_mid = MemberId::from(authed_pid.clone());
    if !is_member(&membership, &authed_mid, authed_kind) {
        return Err(ApiError::not_found("unknown room"));
    }
    // A member may send only as themselves. A member already knows the room
    // exists, so a 403 here (not a 404) leaks nothing.
    if env.sender.id != authed_pid || env.sender.kind != authed_kind {
        return Err(ApiError::forbidden("envelope sender does not match auth"));
    }

    // Stamp the authoritative sender identity before persist + fan-out, retiring
    // the advisory client-supplied `Participant.handle`. The relay is the sole
    // source of room identity: a tagma cannot self-declare its label/owner (the
    // profile is registry-resolved), and a human's name comes from the verified
    // session. Built once here so the durable row and the live event agree.
    env.sender.handle = match &principal {
        Principal::Tagma(tid) => match state.agent_profiles.get(&authed_pid) {
            Some(p) => agent_handle(&authed_pid, &p.owner_username),
            None => match crate::control_policy::tagma_profile(&*state.control, tid).await {
                Ok(Some(p)) if crate::control_policy::tunnel_usable(&p) => {
                    let profile = AgentProfile {
                        label: p.label,
                        owner_username: p.owner_username,
                        owner_display_name: p.owner_display_name,
                    };
                    let h = agent_handle(&authed_pid, &profile.owner_username);
                    state.agent_profiles.set(authed_pid.clone(), profile);
                    h
                }
                // Unknown / not usable (pending, revoked, no pinned key) / a
                // transient registry error: degrade to the unforgeable id-prefix
                // only -- never the tagma-supplied handle, and never a 500 (a
                // registry outage must not fail the send). `degraded_handle` is
                // shared with the history-read fallback, so the vocabulary has
                // one home.
                _ => degraded_handle(&authed_pid, ParticipantKind::Agent),
            },
        },
        Principal::User(_) => match &user_display {
            // Stable handle = the user's login @username (NOT their mutable
            // display name, which is prepended at render).
            Some(d) => human_handle(&d.username),
            None => degraded_handle(&authed_pid, ParticipantKind::Human),
        },
        Principal::Admin => unreachable!("admin is not a room participant"),
    };

    // The agent's tagma_id rides the live envelope (and the history read re-derives it)
    // so a message header can deep-link to that tagma's profile without reversing the
    // one-way participant id. Humans carry none.
    env.sender.tagma_id = match &principal {
        Principal::Tagma(tid) => Some(tid.clone()),
        _ => None,
    };

    // Persist the room payload (plaintext `RoomMessage` JSON), stored opaquely
    // as bytes. The lesche is the room's store of record and is trusted to read
    // room content. Only the sender's stable identity (`id` + `kind`) is
    // persisted; the stamped `handle` rides the live fan-out below and is
    // re-derived from the registry on history read, so no display handle is
    // frozen into the row.
    store::append(
        db,
        room.as_ref(),
        &authed_mid,
        env.sender.kind,
        membership.membership_epoch,
        &env.ciphertext.0,
    )
    .await
    .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;

    // Fan to every other member under the registry read lock. The `membership`
    // snapshot is read once above (no lock on the room row), so a member removed
    // by a concurrent mutation in this window may receive this one extra live
    // envelope -- self-healing (the durable row is correct and the next fan-out
    // excludes them); not worth a row lock on the send hot path.
    let _out = {
        let reg = state.read()?;
        fan_envelope(&reg, &membership, &env)
    };

    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize, Default)]
struct HistoryQuery {
    /// Return messages with `seq > after_seq` (exclusive). Default 0 = from the
    /// start.
    after_seq: Option<i64>,
    /// Max messages to return. Server-clamped.
    limit: Option<u64>,
}

/// Server cap on a single history page, defending an unbounded client `limit`.
const HISTORY_MAX_LIMIT: u64 = 200;

#[derive(Debug, Serialize)]
struct StoredMessageView {
    seq: i64,
    sender: Participant,
    epoch: i64,
    ciphertext: Ciphertext,
    #[serde(with = "time::serde::iso8601")]
    created_at: OffsetDateTime,
}

/// Pull a room's message history. Member-only (user or tagma principal must
/// belong to the room). Returns stored rows whose payload is the plaintext
/// `RoomMessage` JSON (the lesche is the room's store of record and is trusted
/// to read room content).
async fn room_history(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(room_id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Vec<StoredMessageView>>, ApiError> {
    let room = RoomId::from(room_id);
    let db = state.require_db()?;
    let membership = store::room_membership(db, room.as_ref())
        .await
        .map_err(map_db_err)?
        .ok_or_else(|| ApiError::not_found("unknown room"))?;
    // Member-only: existence-oracle 404 for non-members. The room surface deals
    // only in participant ids + kinds.
    let (authed_mid, authed_kind) = match (
        principal.participant_id().map(MemberId::from),
        principal.participant_kind(),
    ) {
        (Some(mid), Some(kind)) => (mid, kind),
        _ => return Err(ApiError::not_found("unknown room")), // Admin
    };
    if !is_member(&membership, &authed_mid, authed_kind) {
        return Err(ApiError::not_found("unknown room"));
    }

    let limit = q.limit.unwrap_or(HISTORY_MAX_LIMIT).min(HISTORY_MAX_LIMIT);
    let rows = store::read_since(db, room.as_ref(), q.after_seq.unwrap_or(0), limit)
        .await
        .map_err(|e| ApiError::internal(format_args!("store error: {e}")))?;
    let views = resolve_history_views(&state, db, room.as_ref(), rows).await?;
    Ok(Json(views))
}

/// Resolve each history row's sender display handle fresh from the registry,
/// then assemble the wire views. The row carries only the sender's stable
/// `ParticipantId` + kind; the handle is derived here (consistent with the
/// roster, which uses the same `resolve_handles`).
///
/// Source-id recovery: a sender's `user_id`/`tagma_id` (what the registry
/// resolves by) is fetched from live `room_members` OR the
/// `room_member_revocations` audit -- a sender who left after sending is
/// gone from the live table but retained in the audit, so departed senders keep
/// their real `@owner` handle. A sender in NEITHER (exceptional data gap), or a
/// registry RPC failure, degrades to the unforgeable `<kind> <short_prefix>`
/// fallback -- a registry blip never blanks a history pull.
async fn resolve_history_views(
    state: &SharedConvState,
    db: &crate::db::Db,
    room: &str,
    rows: Vec<store::StoredMessage>,
) -> Result<Vec<StoredMessageView>, ApiError> {
    use std::collections::HashMap;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Distinct senders + each sender's kind (a member has one kind).
    let mut kind_by_mid: HashMap<String, ParticipantKind> = HashMap::new();
    for r in &rows {
        kind_by_mid
            .entry(r.sender_id.as_ref().to_string())
            .or_insert(r.sender_kind);
    }
    let distinct_mids: Vec<String> = {
        let mut s: Vec<String> = kind_by_mid.keys().cloned().collect();
        s.sort();
        s
    };

    let source_map = store::member_source_map(db, room, &distinct_mids)
        .await
        .map_err(map_db_err)?;

    // Build refs only for senders we can resolve to a source id; senders in
    // neither table (an exceptional data gap) are omitted and degrade via the
    // `unwrap_or_else` in the assembly below.
    let refs: Vec<MemberRef> = distinct_mids
        .iter()
        .filter_map(|mid_s| {
            // `kind_by_mid` is built from `distinct_mids`, so the key is always
            // present; the unwrap_or is defensive only.
            let kind = kind_by_mid
                .get(mid_s)
                .copied()
                .unwrap_or(ParticipantKind::Agent);
            source_map.get(mid_s).map(|source_id| MemberRef {
                id: MemberId::from(mid_s.clone()),
                kind,
                source_id: source_id.clone(),
            })
        })
        .collect();

    // Resolve; on a registry error degrade every sender (return an empty map so
    // the assembly falls back per-sender) rather than failing the history read.
    let resolved: HashMap<MemberId, _> = match resolve_handles(&*state.control, &refs).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "history sender resolve failed; degrading handles");
            HashMap::new()
        }
    };

    let views = rows
        .into_iter()
        .map(|r| {
            let id = resolved
                .get(&r.sender_id)
                .cloned()
                .unwrap_or_else(|| degraded(&r.sender_id, r.sender_kind));
            StoredMessageView {
                seq: r.seq,
                sender: Participant {
                    id: r.sender_id.into(),
                    kind: r.sender_kind,
                    handle: id.handle,
                    tagma_id: id.tagma_id,
                },
                epoch: r.epoch,
                ciphertext: Ciphertext(r.ciphertext),
                created_at: r.created_at,
            }
        })
        .collect();
    Ok(views)
}

#[cfg(test)]
mod tests {
    //! Room envelope: a member's envelope is fanned to the other members; a
    //! non-member sender is 404; an unknown room is 404.

    use super::*;
    use crate::auth::AuthPrincipal;
    use crate::routes::test_support::{db_state, seed_room};
    use kallip_agora_common::bytes::Ciphertext;
    use kallip_agora_common::ids::{
        ConversationId, ParticipantId, ParticipantKind, TagmaId, TraceId, UserId,
    };
    use kallip_agora_common::principal::Principal;
    use kallip_lesche_common::event::LescheEvent;
    use kallip_lesche_common::tunnel::TunnelInbound;
    use time::OffsetDateTime;

    fn envelope(sender: Participant, room: &str) -> Envelope {
        Envelope {
            conversation_id: ConversationId::from(room.to_string()),
            sender,
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(vec![1u8; 12]),
        }
    }

    fn human(handle: &str, user: &str) -> Participant {
        Participant {
            id: ParticipantId::for_user(&UserId::from(user.to_string())),
            kind: ParticipantKind::Human,
            handle: handle.to_string(),
            tagma_id: None,
        }
    }

    fn agent(handle: &str, tagma: &TagmaId) -> Participant {
        Participant {
            id: ParticipantId::for_tagma(tagma),
            kind: ParticipantKind::Agent,
            handle: handle.to_string(),
            tagma_id: None,
        }
    }

    fn uid(s: &str) -> UserId {
        UserId::from(s.to_string())
    }

    /// Open `user`'s app stream and return a receiver (kept alive by the
    /// caller) so a fan send lands on a live subscriber.
    fn app_rx(
        state: &SharedConvState,
        user: &UserId,
    ) -> tokio::sync::broadcast::Receiver<LescheEvent> {
        let mut reg = state.write().unwrap();
        reg.open_app_stream(user).subscribe()
    }

    /// Register a live tagma tunnel and return its receiver.
    fn tunnel_rx(
        state: &SharedConvState,
        tagma: &TagmaId,
        owner: &UserId,
    ) -> tokio::sync::broadcast::Receiver<TunnelInbound> {
        let mut reg = state.registry.write().unwrap();
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        let rx = tx.subscribe();
        reg.register_presence(tagma, owner.clone(), tx, std::sync::Arc::new(()));
        rx
    }

    fn participant(s: &str) -> AuthPrincipal {
        AuthPrincipal(Principal::User(uid(s)))
    }

    /// The verified-session display the AuthPrincipal extractor would stash for
    /// a cookie-authed `s` user (username = `s`, no display name).
    fn opt_user(s: &str) -> OptUserDisplay {
        OptUserDisplay(Some(crate::auth::UserDisplay {
            username: s.to_string(),
            display_name: None,
        }))
    }

    #[tokio::test]
    async fn member_envelope_is_fanned_to_other_members() {
        let (state, _control, _container) = db_state().await;
        let alice = uid("alice");
        let bob = uid("bob");
        let t1 = TagmaId::from("t1".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[&bob],
            &[&t1],
        )
        .await;

        let mut bobs_rx = app_rx(&state, &bob);
        let mut t1_rx = tunnel_rx(&state, &t1, &alice);

        let env = envelope(human("Alice", "alice"), "room-1");
        let status = post_room_envelope(
            State(state),
            participant("alice"),
            opt_user("alice"),
            Path("room-1".to_string()),
            Json(env),
        )
        .await
        .expect("accepted");
        assert_eq!(status, StatusCode::ACCEPTED);

        // bob's app stream and t1's tunnel each received the envelope.
        let bob_ev = bobs_rx.recv().await.expect("bob received the envelope");
        let _ = t1_rx.recv().await;

        // Security: the relay stamps the authoritative STABLE handle, never the
        // client-supplied one. The sender sent handle "Alice"; the fan must
        // carry "@alice" (the stable username handle), proving a client cannot
        // spoof a display handle into the room.
        let kallip_lesche_common::event::LescheEvent::Envelope { envelope } = bob_ev else {
            panic!("expected an Envelope event");
        };
        assert_eq!(
            envelope.sender.handle, "@alice",
            "relay overwrites the client-supplied handle with the stable @username"
        );
    }

    #[tokio::test]
    async fn agent_envelope_is_stamped_with_stable_owner_handle() {
        let (state, control, _container) = db_state().await;
        let alice = uid("alice");
        let t1 = TagmaId::from("t1".to_string());
        // Enroll t1 so its identity (owner username) resolves at send time. The
        // owner's username is "alice" (MockControlPlane seeds it from the id).
        control.enroll_tagma(
            &t1,
            alice.clone(),
            kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
            "tagma-token",
        );
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;

        // The owner's app stream receives the fanned envelope.
        let mut alice_rx = app_rx(&state, &alice);

        // The tagma sends with a SPOOFED handle "Evil"; the relay must overwrite
        // it with the stable `<id-prefix>@<owner>` handle.
        let env = envelope(agent("Evil", &t1), "room-1");
        let _ = post_room_envelope(
            State(state),
            AuthPrincipal(Principal::Tagma(t1.clone())),
            OptUserDisplay(None),
            Path("room-1".to_string()),
            Json(env),
        )
        .await
        .expect("accepted");

        let ev = alice_rx.recv().await.expect("alice received the envelope");
        let LescheEvent::Envelope { envelope } = ev else {
            panic!("expected an Envelope event");
        };
        let pid = ParticipantId::for_tagma(&t1);
        let prefix = &pid.as_ref()[..6];
        assert_eq!(
            envelope.sender.handle,
            format!("{}@alice", prefix),
            "agent handle is the stable id-prefix@owner"
        );
        assert_ne!(
            envelope.sender.handle, "Evil",
            "a tagma-supplied handle must not survive relay stamping"
        );
        // The relay also stamps the agent's tagma_id on the live envelope so a
        // message header can deep-link to the tagma profile.
        assert_eq!(
            envelope.sender.tagma_id,
            Some(t1.clone()),
            "agent sender carries its tagma_id"
        );
    }

    /// A not-usable sender tagma (revoked, or pending with no pinned key) on a
    /// cache miss degrades to the unforgeable `agent <prefix>` handle -- never
    /// the tagma-supplied handle, and the send still succeeds (no 500).
    #[tokio::test]
    async fn agent_envelope_degrades_to_prefix_when_not_usable() {
        let (state, control, _container) = db_state().await;
        let alice = uid("alice");
        let revoked = TagmaId::from("rev".to_string());
        control.enroll_tagma(
            &revoked,
            alice.clone(),
            kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
            "tok-rev",
        );
        control.revoke_tagma(&revoked);
        let pending = TagmaId::from("pen".to_string());
        control.enroll_tagma(
            &pending,
            alice.clone(),
            kallip_agora_common::bytes::Ed25519PublicKey(vec![2u8; 32]),
            "tok-pen",
        );
        control.set_pinned_key(&pending, None);
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[],
            &[&revoked, &pending],
        )
        .await;

        let mut alice_rx = app_rx(&state, &alice);

        for tagma in [&revoked, &pending] {
            let pid = ParticipantId::for_tagma(tagma);
            let prefix = pid.as_ref()[..6].to_string();
            let _ = post_room_envelope(
                State(state.clone()),
                AuthPrincipal(Principal::Tagma(tagma.clone())),
                OptUserDisplay(None),
                Path("room-1".to_string()),
                Json(envelope(agent("Evil", tagma), "room-1")),
            )
            .await
            .expect("accepted even when the sender is not usable");
            let ev = alice_rx.recv().await.expect("alice received the envelope");
            let LescheEvent::Envelope { envelope } = ev else {
                panic!("expected an Envelope event");
            };
            assert_eq!(
                envelope.sender.handle,
                format!("agent {}", prefix),
                "a not-usable sender degrades to the unforgeable id-prefix"
            );
        }
    }

    #[tokio::test]
    async fn non_member_sender_is_404() {
        let (state, _control, _container) = db_state().await;
        let alice = uid("alice");
        let t1 = TagmaId::from("t1".to_string());
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;
        // carol is not a member.
        let env = envelope(human("Carol", "carol"), "room-1");
        let err = post_room_envelope(
            State(state),
            participant("carol"),
            opt_user("carol"),
            Path("room-1".to_string()),
            Json(env),
        )
        .await
        .expect_err("non-member rejected");
        assert_eq!(err.status, 404);
    }

    /// Existence-oracle: a non-member cannot probe a room's existence by
    /// spoofing another member's sender id. The membership gate runs BEFORE the
    /// sender-match, so a non-member gets the same 404 as for an unknown room --
    /// never a 403 that would confirm the room is real.
    #[tokio::test]
    async fn non_member_spoofed_sender_is_404_not_403() {
        let (state, _control, _container) = db_state().await;
        let alice = uid("alice");
        let t1 = TagmaId::from("t1".to_string());
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;
        // carol is not a member, but spoofs alice's sender id on the envelope.
        let env = envelope(human("Alice", "alice"), "room-1");
        let err = post_room_envelope(
            State(state),
            participant("carol"),
            opt_user("carol"),
            Path("room-1".to_string()),
            Json(env),
        )
        .await
        .expect_err("non-member rejected even with a spoofed sender");
        assert_eq!(err.status, 404);
    }

    /// A member may send only as themselves: alice (a member) spoofing bob's
    /// sender id is rejected. This locks the post-gate sender-match so a future
    /// reorder cannot silently drop enforcement while the non-member oracle test
    /// above still passes.
    #[tokio::test]
    async fn member_spoofing_another_member_is_403() {
        let (state, _control, _container) = db_state().await;
        let alice = uid("alice");
        let bob = uid("bob");
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[&bob], &[]).await;
        // alice is a member but spoofs bob's sender id.
        let env = envelope(human("Bob", "bob"), "room-1");
        let err = post_room_envelope(
            State(state),
            participant("alice"),
            opt_user("alice"),
            Path("room-1".to_string()),
            Json(env),
        )
        .await
        .expect_err("member spoofing another member rejected");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn unknown_room_is_404() {
        let (state, _control, _container) = db_state().await;
        let env = envelope(human("Alice", "alice"), "ghost");
        let err = post_room_envelope(
            State(state),
            participant("alice"),
            opt_user("alice"),
            Path("ghost".to_string()),
            Json(env),
        )
        .await
        .expect_err("unknown room");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn history_member_empty_and_non_member_is_404() {
        let (state, _control, _container) = db_state().await;
        let alice = uid("alice");
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[]).await;
        // Member: 200 with empty history.
        let Json(rows) = room_history(
            State(state.clone()),
            participant("alice"),
            Path("room-1".to_string()),
            Query(HistoryQuery::default()),
        )
        .await
        .expect("member history");
        assert!(rows.is_empty());

        // Non-member: 404.
        let err = room_history(
            State(state),
            participant("carol"),
            Path("room-1".to_string()),
            Query(HistoryQuery::default()),
        )
        .await
        .expect_err("non-member rejected");
        assert_eq!(err.status, 404);
    }

    /// Simulate a member leaving: hard-delete from `room_members` + append
    /// to the `room_member_revocations` audit (the live/audit split a real
    /// removal performs). Used to prove history still resolves a departed
    /// sender's handle from the audit, not the gone live row.
    async fn remove_member(
        db: &crate::db::Db,
        room: &str,
        pid: &ParticipantId,
        kind: ParticipantKind,
        source_id: &str,
    ) {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM room_members WHERE room_id = $1 AND member_id = $2",
            [room.into(), pid.as_ref().to_string().into()],
        ))
        .await
        .expect("delete live member");
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO room_member_revocations \
             (id, room_id, member_id, kind, source_id, revoked_by, revoked_at, reason) \
             VALUES ($1::uuid, $2, $3, $4, $5, 'revoker', NOW(), 'test')",
            [
                uuid::Uuid::new_v4().to_string().into(),
                room.into(),
                pid.as_ref().to_string().into(),
                kind.as_str().into(),
                source_id.into(),
            ],
        ))
        .await
        .expect("insert revocation audit");
    }

    /// History read resolves each sender's display handle FRESH from the
    /// registry -- the row stores only the stable `ParticipantId`, so the
    /// handle matches the roster and is never a stale send-time snapshot. The
    /// client-supplied handles ("Alice" / "Evil") do not survive; the stable
    /// `@username` / `<prefix>@owner` forms are derived at read.
    #[tokio::test]
    async fn history_resolves_sender_handles_from_registry() {
        let (state, control, _container) = db_state().await;
        let alice = uid("alice");
        let t1 = TagmaId::from("t1".to_string());
        // Enrolling t1 owned by alice also seeds alice's user identity
        // (username "alice"), which the human resolve needs.
        control.enroll_tagma(
            &t1,
            alice.clone(),
            kallip_agora_common::bytes::Ed25519PublicKey(vec![1u8; 32]),
            "tagma-token",
        );
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[], &[&t1]).await;

        // alice (human) sends with a spoofed handle, then t1 (agent) sends with
        // a spoofed handle.
        let _ = post_room_envelope(
            State(state.clone()),
            participant("alice"),
            opt_user("alice"),
            Path("room-1".to_string()),
            Json(envelope(human("Alice", "alice"), "room-1")),
        )
        .await
        .expect("alice send");
        let _ = post_room_envelope(
            State(state.clone()),
            AuthPrincipal(Principal::Tagma(t1.clone())),
            OptUserDisplay(None),
            Path("room-1".to_string()),
            Json(envelope(agent("Evil", &t1), "room-1")),
        )
        .await
        .expect("agent send");

        // Pull history as alice (a member).
        let Json(rows) = room_history(
            State(state),
            participant("alice"),
            Path("room-1".to_string()),
            Query(HistoryQuery::default()),
        )
        .await
        .expect("history");
        assert_eq!(rows.len(), 2);
        // Human sender: stable @username, NOT the client-supplied "Alice".
        assert_eq!(rows[0].sender.handle, "@alice");
        assert_eq!(rows[0].sender.kind, ParticipantKind::Human);
        assert_eq!(
            rows[0].sender.tagma_id, None,
            "human sender has no tagma_id"
        );
        // Agent sender: stable <id-prefix>@owner, NOT the spoofed "Evil".
        let prefix = ParticipantId::for_tagma(&t1).as_ref()[..6].to_string();
        assert_eq!(rows[1].sender.handle, format!("{}@alice", prefix));
        assert_eq!(rows[1].sender.kind, ParticipantKind::Agent);
        assert_eq!(
            rows[1].sender.tagma_id,
            Some(t1.clone()),
            "agent sender carries its tagma_id"
        );
    }

    /// A departed sender is gone from the live membership but retained in the
    /// `room_member_revocations` audit; history read resolves the real
    /// `@owner` handle via the audit rather than degrading to a bare prefix.
    #[tokio::test]
    async fn history_resolves_a_departed_sender_via_revocations() {
        let (state, control, _container) = db_state().await;
        let alice = uid("alice");
        let bob = uid("bob");
        // Seed bob's identity so the registry resolves him to "@bob" after he
        // leaves (he is no tagma owner, so enroll_tagma does not seed him).
        control.seed_user(bob.clone());
        seed_room(state.db.as_ref().unwrap(), "room-1", &alice, &[&bob], &[]).await;

        // bob sends, then leaves the room.
        let bob_pid = ParticipantId::for_user(&bob);
        let _ = post_room_envelope(
            State(state.clone()),
            participant("bob"),
            opt_user("bob"),
            Path("room-1".to_string()),
            Json(envelope(human("Bob", "bob"), "room-1")),
        )
        .await
        .expect("bob send");
        remove_member(
            state.db.as_ref().unwrap(),
            "room-1",
            &bob_pid,
            ParticipantKind::Human,
            "bob",
        )
        .await;

        // alice pulls history: bob is gone from the live membership, but his
        // message remains and its handle resolves via the revocations audit.
        let Json(rows) = room_history(
            State(state),
            participant("alice"),
            Path("room-1".to_string()),
            Query(HistoryQuery::default()),
        )
        .await
        .expect("history");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].sender.handle, "@bob",
            "departed sender resolved via revocations, not degraded to a prefix"
        );
    }
}
