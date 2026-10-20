//! Tagma unread relay fan: presence-cache write + owner-stream broadcast.
//!
//! The batched `POST /tagmata/{tagma_id}/upstream` channel demultiplexes
//! Unread elements into [`relay_unread`], which caches the snapshot on the
//! tunnel entry and rebroadcasts it as a [`KeryxEvent::TagmaUnread`] on the
//! owner's app event stream. The snapshot is plaintext and user-scoped: the
//! relay learns *that* a user has read up to a row of a conversation, never
//! what the row says (message content stays inside the E2EE envelope). It is
//! also the reason the badge can drop on a device the user was not reading
//! on — the count is the tagma's, and every device of the owner subscribes to
//! the same app stream.
//!
//! Why not ride the envelope: the tagma holds one `CryptoState` per tagma
//! conversation, while every client open generates a fresh ephemeral key and
//! re-KEXes the pair, so a server-side push has no key that all of the user's
//! devices share. Plaintext upstream is the same trust decision the room read
//! cursors already make.
//!
//! Concurrency: the cache write runs under a registry WRITE lock (it mutates
//! `PresenceEntry::latest_unread`; broadcast `send` is synchronous), never
//! co-held with a `ControlPlane` call.
//!
//! Ownership gate: the relay-side badge belongs to one peer per tagma,
//! the owner's own user participant. The operator partition (peer `""`)
//! and any other user's partition are dropped before the cache: they are
//! not this user's numbers, and a `ParticipantId` is a one-way hash of
//! the user id, so their streams are not addressable from the peer.

use axum::http::StatusCode;
use kallipai_archeion_common::ids::{ParticipantId, TagmaId};
use kallipai_common::protocol::ApiError;
use kallipai_keryx_common::event::KeryxEvent;
use kallipai_lesche_common::event::UnreadSnapshot;
use tracing::debug;

use crate::state::{OwnerUnread, SharedConvState};

/// The batched upstream channel's unread fan: presence-cache write +
/// owner-stream broadcast. Single shared implementation so wire paths cannot
/// drift.
pub(crate) async fn relay_unread(
    state: &SharedConvState,
    tagma_id: TagmaId,
    snapshot: UnreadSnapshot,
) -> Result<StatusCode, ApiError> {
    // Same routing as the status fan: the tunnel must be live (the flusher
    // only runs while it is), so a missing presence entry is a routing gap
    // worth surfacing rather than masking. No await inside the critical
    // section.
    let app_stream = {
        let mut reg = state.write()?;
        let entry = reg
            .presence_by_tagma_mut(&tagma_id)
            .ok_or_else(|| ApiError::not_found("no live tunnel for tagma"))?;
        let owner = entry.owner.clone();
        // The one peer this tunnel's badge can belong to. Two partitions
        // exist in practice (`""` for the operator's direct path and the
        // owner's user partition for the relay); only the latter is this
        // user's number, so the single-slot cache holds exactly one peer.
        if snapshot.peer != ParticipantId::for_user(&owner).as_ref() {
            debug!(
                tagma = %tagma_id,
                peer = %snapshot.peer,
                "unread snapshot outside the owner's partition dropped"
            );
            // 202 like the no-subscriber path: a retry cannot change the
            // answer, and that partition's reader pulls its own count.
            return Ok(StatusCode::ACCEPTED);
        }
        // Unconditional cache write: the count must land even with no live
        // subscribers, or a reconnect's flush starts from the pre-gap value.
        entry.latest_unread = Some(OwnerUnread {
            cursor: snapshot.cursor,
            unread: snapshot.unread,
        });
        reg.app_stream(&owner)
    };

    // No live app stream -> silent drop (best-effort). Still 202 so the tagma
    // does not retry: the next cursor write or outbound reply re-announces the
    // number, so a dropped frame costs staleness, never a lost count.
    if let Some(stream) = app_stream
        && stream
            .deliver(KeryxEvent::TagmaUnread {
                tagma_id: tagma_id.clone(),
                peer: snapshot.peer,
                cursor: snapshot.cursor,
                unread: snapshot.unread,
            })
            .is_ok()
    {
        debug!(tagma = %tagma_id, "unread relayed");
    }
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallipai_archeion_common::bytes::Ed25519PublicKey;
    use kallipai_archeion_common::ids::{ParticipantId, UserId};

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    /// The cache write is unconditional for the owner's own partition: a
    /// client that connects after the frame (no live subscriber at relay
    /// time) still gets the number from the connect flush, so a cold-start
    /// connect badges it.
    #[tokio::test]
    async fn relay_unread_caches_snapshot_without_subscribers() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        let status = relay_unread(
            &state,
            tagma.clone(),
            UnreadSnapshot {
                peer: ParticipantId::for_user(&owner).as_ref().to_string(),
                cursor: 42,
                unread: 3,
            },
        )
        .await
        .expect("relay ok");
        assert_eq!(status, StatusCode::ACCEPTED);
        let cached = state
            .read()
            .unwrap()
            .presence_by_tagma(&tagma)
            .and_then(|e| e.latest_unread)
            .expect("snapshot cached despite no subscribers");
        assert_eq!(cached.cursor, 42);
        assert_eq!(cached.unread, 3);
    }

    #[tokio::test]
    async fn relay_unread_no_live_tunnel_404() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let err = relay_unread(
            &state,
            TagmaId::from("ghost".to_string()),
            UnreadSnapshot {
                peer: String::new(),
                cursor: 0,
                unread: 0,
            },
        )
        .await
        .expect_err("no tunnel 404");
        assert_eq!(err.status, 404);
    }
    /// The operator partition (peer `""`, the direct path) is not this user's
    /// number: it is dropped before the cache and the owner's stream.
    #[tokio::test]
    async fn relay_unread_drops_the_operator_partition() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        assert_dropped(
            &state,
            &tagma,
            &owner,
            UnreadSnapshot {
                peer: String::new(),
                cursor: 5,
                unread: 4,
            },
        )
        .await;
    }
    /// Another user's partition is not this owner's number either, and the
    /// relay cannot address that user's stream from the peer string (a
    /// `ParticipantId` is a one-way hash), so it is dropped the same way.
    #[tokio::test]
    async fn relay_unread_drops_a_foreign_users_partition() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        assert_dropped(
            &state,
            &tagma,
            &owner,
            UnreadSnapshot {
                peer: ParticipantId::for_user(&user("stranger"))
                    .as_ref()
                    .to_string(),
                cursor: 9,
                unread: 7,
            },
        )
        .await;
    }

    /// The badge is one number: a later snapshot from another partition
    /// must not overwrite the owner's cached one.
    #[tokio::test]
    async fn relay_unread_keeps_the_owner_count_when_other_partitions_land() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        let mut rx = state.write().unwrap().open_app_stream(&owner).subscribe();
        relay_unread(
            &state,
            tagma.clone(),
            UnreadSnapshot {
                peer: ParticipantId::for_user(&owner).as_ref().to_string(),
                cursor: 12,
                unread: 12,
            },
        )
        .await
        .expect("relay ok");
        for peer in [
            String::new(),
            ParticipantId::for_user(&user("stranger"))
                .as_ref()
                .to_string(),
        ] {
            relay_unread(
                &state,
                tagma.clone(),
                UnreadSnapshot {
                    peer,
                    cursor: 99,
                    unread: 99,
                },
            )
            .await
            .expect("relay ok");
        }
        let cached = state
            .read()
            .unwrap()
            .presence_by_tagma(&tagma)
            .and_then(|e| e.latest_unread)
            .expect("owner snapshot cached");
        assert_eq!((cached.cursor, cached.unread), (12, 12));
        // Exactly one broadcast: the owner's own snapshot.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    /// A snapshot outside the owner's partition: accepted (a retry cannot
    /// change the answer), never cached, never broadcast.
    async fn assert_dropped(
        state: &SharedConvState,
        tagma: &TagmaId,
        owner: &UserId,
        snapshot: UnreadSnapshot,
    ) {
        let mut rx = state.write().unwrap().open_app_stream(owner).subscribe();
        let status = relay_unread(state, tagma.clone(), snapshot)
            .await
            .expect("relay ok");
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(
            state
                .read()
                .unwrap()
                .presence_by_tagma(tagma)
                .and_then(|e| e.latest_unread)
                .is_none(),
            "a foreign partition is not cached"
        );
        assert!(rx.try_recv().is_err(), "nor broadcast");
    }
}
