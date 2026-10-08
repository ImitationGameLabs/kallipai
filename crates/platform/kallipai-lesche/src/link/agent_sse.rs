//! The agent SSE stream: a tagma's delivery carrier for room and direct-
//! session envelopes and the membership wake. Authentication is the
//! tagma's long-lived bearer alone (no per-connect device proof), and the
//! stream carries only delivery-class frames: no key exchange, no
//! manage proxy, no owner-subscription hints (those ride the keryx
//! tunnel).
//! Delivery reads this stream directly: a room or direct fan-out delivers
//! here, and the delivered/missed tally follows the agent stream's
//! liveness. Room presence is this stream's own connect/drop edge
//! (online on connect, offline on drop).
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use kallipai_archeion_common::ids::{ParticipantId, TagmaId};
use kallipai_common::protocol::ApiError;
use kallipai_lesche_common::event::LescheAgentEvent;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_tagma};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::{BROADCAST_CAPACITY, Registry, SharedConvState};

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/tagmata/{id}/events", get(agent_events))
}

/// Retire an agent stream at connection teardown: remove the slot iff the
/// live entry is still `id` (the ownership token). This stream is the
/// tagma's room-presence source, so a retire is the offline edge.
pub(super) fn retire_agent_stream(reg: &mut Registry, tagma: &TagmaId, id: &Arc<()>) -> bool {
    let removed = reg.take_agent_stream_if_owned(tagma, id);
    if removed {
        tracing::info!(tagma = %tagma, "agent stream closed; slot removed");
    }
    removed
}

async fn agent_events(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Sse<axum::response::sse::KeepAliveStream<OnDrop>>, ApiError> {
    let tagma_id = TagmaId::from(id);
    let caller = require_tagma(&principal)?.clone();
    // Self-match: the consuming principal is the tagma itself. A valid bearer
    // naming a different tagma collapses to the existence-oracle 404 (same
    // shape as the keryx tunnel gate); user/admin principals fail
    // `require_tagma`.
    if caller != tagma_id {
        return Err(ApiError::not_found("unknown tagma"));
    }
    // Capture the runtime handle so the synchronous `OnDrop` cleanup (which
    // may run off-runtime, e.g. during body teardown) can spawn the presence
    // fan-out -- never under a registry lock.
    let handle = tokio::runtime::Handle::try_current().ok();

    // Reserve the agent-stream slot. One live stream per tagma.
    let (tx, rx) = broadcast::channel::<LescheAgentEvent>(BROADCAST_CAPACITY);
    let slot_id = Arc::new(());
    {
        let mut reg = state.write()?;
        if !reg.register_agent_stream(&tagma_id, tx.clone(), slot_id.clone()) {
            return Err(ApiError::conflict("tagma already has a live agent stream"));
        }
    }
    // Room-member presence edge on connect: an idempotent best-effort hint
    // (viewers treat presence as a set), so the hint stays timely.
    if let Some(h) = &handle {
        let st = state.clone();
        let who = ParticipantId::for_tagma(&tagma_id);
        h.spawn(async move {
            crate::chat::fan_member_presence(&st, &who, true).await;
        });
    }
    tracing::info!(tagma = %tagma_id, "agent stream established");

    let lag_tagma = tagma_id.clone();
    let stream: BoxEventStream = Box::pin(
        BroadcastStream::new(rx)
            .filter_map(move |r| match r {
                Ok(env) => Some(env),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    tracing::warn!(lag = n, tagma = %lag_tagma, "agent stream lagged; events dropped");
                    None
                }
            })
            .map(|ev| {
                Ok::<Event, std::convert::Infallible>(
                    Event::default()
                        .json_data(ev)
                        .expect("agent event serializes"),
                )
            }),
    );

    // Synchronous cleanup in Drop::drop: retire the slot under the registry
    // write lock (the suppression check rides the same lock, so a stream
    // registering concurrently cannot race it), then spawn the async
    // room-presence fan-out off-thread.
    let cleanup_state = state.clone();
    let cleanup_tagma = tagma_id.clone();
    let cleanup_id = slot_id.clone();
    let cleanup_handle = handle.clone();
    let cleaned = OnDrop::new(stream, move || {
        let fire_offline = {
            let Ok(mut reg) = cleanup_state.write() else {
                return;
            };
            retire_agent_stream(&mut reg, &cleanup_tagma, &cleanup_id)
        };
        if fire_offline {
            if let Some(h) = cleanup_handle.as_ref() {
                let st = cleanup_state.clone();
                let who = ParticipantId::for_tagma(&cleanup_tagma);
                h.spawn(async move {
                    crate::chat::fan_member_presence(&st, &who, false).await;
                });
            } else {
                tracing::warn!("offline-presence fan skipped: no runtime at drop");
            }
        }
    });
    // Server keepalive (a comment frame every 15 s) keeps both ends able to
    // tell a live stream from a half-open one.
    Ok(Sse::new(cleaned).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_state;
    use axum::response::IntoResponse;
    use kallipai_archeion_common::bytes::Ciphertext;
    use kallipai_archeion_common::ids::{ChannelId, ParticipantKind, TraceId, UserId};
    use kallipai_archeion_common::principal::Principal;
    use kallipai_lesche_common::message::{Envelope, Participant};
    use kallipai_lesche_common::rooms::MemberId;
    use time::OffsetDateTime;

    fn as_tagma(t: &TagmaId) -> AuthPrincipal {
        AuthPrincipal(Principal::Tagma(t.clone()))
    }

    /// Build a minimal envelope (ciphertext/seq are irrelevant to routing).
    fn envelope(tagma: &TagmaId) -> Envelope {
        Envelope {
            channel_id: ChannelId::from(format!("c-{tagma}")),
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("owner".to_string())),
                kind: ParticipantKind::Human,
                handle: "owner".into(),
                tagma_id: None,
            },
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        }
    }

    /// Pull the next `data:` frame's payload off an established stream.
    async fn next_data(body: &mut axum::body::BodyDataStream) -> Option<String> {
        let frame = futures_util::StreamExt::next(body).await?;
        let bytes = frame.expect("frame bytes");
        let text = String::from_utf8_lossy(&bytes);
        text.lines()
            .find(|l| l.starts_with("data:"))
            .map(|l| l[5..].trim().to_string())
    }

    /// A cookie-authenticated user cannot reach the stream: `require_tagma` is
    /// the deputy guard (403 for any non-tagma principal; anonymous requests
    /// never reach the handler -- the extractor answers 401).
    #[tokio::test]
    async fn non_tagma_principals_are_rejected() {
        let (state, _control) = make_state();
        let err = agent_events(
            State(state),
            AuthPrincipal(Principal::User(UserId::from("op".to_string()))),
            Path("t-a".to_string()),
        )
        .await
        .expect_err("user principal rejected");
        assert_eq!(err.status, 403);
        assert_eq!(err.message, "tagma access required");
    }

    /// A valid tagma bearer naming a different tagma gets the existence-
    /// oracle 404 (byte-identical to the unknown-tagma shape).
    #[tokio::test]
    async fn path_mismatch_answers_the_existence_oracle_404() {
        let (state, _control) = make_state();
        let caller = TagmaId::from("t-a".to_string());
        let err = agent_events(State(state), as_tagma(&caller), Path("t-b".to_string()))
            .await
            .expect_err("mismatch rejected");
        assert_eq!(err.status, 404);
        assert_eq!(err.message, "unknown tagma");
    }

    /// One live agent stream per tagma: a second connect answers 409.
    #[tokio::test]
    async fn second_stream_for_one_tagma_is_409() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-409".to_string());
        let _first = agent_events(
            State(state.clone()),
            as_tagma(&tagma),
            Path("t-409".to_string()),
        )
        .await
        .expect("first stream establishes");
        let err = agent_events(State(state), as_tagma(&tagma), Path("t-409".to_string()))
            .await
            .expect_err("second stream rejected");
        assert_eq!(err.status, 409);
    }

    /// A room-style fan delivery lands on the agent stream: the envelope
    /// arrives tagged with the delivery vocabulary's `kind` (`"envelope"`).
    #[tokio::test]
    async fn agent_stream_receives_the_envelope() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-dual".to_string());
        let sse = agent_events(
            State(state.clone()),
            as_tagma(&tagma),
            Path("t-dual".to_string()),
        )
        .await
        .expect("agent stream establishes");
        let mut body = sse.into_response().into_body().into_data_stream();

        let env = envelope(&tagma);
        let delivered = {
            let reg = state.read().unwrap();
            crate::link::send_to_tagma(
                &reg,
                &MemberId::for_tagma(&tagma),
                LescheAgentEvent::Envelope { envelope: env },
            )
        };
        assert!(delivered, "the agent stream is the counted channel");
        let data = tokio::time::timeout(std::time::Duration::from_secs(2), next_data(&mut body))
            .await
            .expect("agent stream yields")
            .expect("data frame present");
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["kind"], "envelope");
        let parsed: LescheAgentEvent = serde_json::from_value(v).unwrap();
        assert!(matches!(parsed, LescheAgentEvent::Envelope { .. }));
    }

    /// The wake hint delivers like envelopes (the delivery vocabulary
    /// has exactly the two kinds).
    #[tokio::test]
    async fn wake_arrives_on_the_stream() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-wake".to_string());
        let sse = agent_events(
            State(state.clone()),
            as_tagma(&tagma),
            Path("t-wake".to_string()),
        )
        .await
        .expect("agent stream establishes");
        let mut body = sse.into_response().into_body().into_data_stream();
        let delivered = {
            let reg = state.read().unwrap();
            crate::link::send_to_tagma(&reg, &MemberId::for_tagma(&tagma), LescheAgentEvent::Wake)
        };
        assert!(delivered, "wake delivers to the live stream");
        let data = tokio::time::timeout(std::time::Duration::from_secs(2), next_data(&mut body))
            .await
            .expect("wake arrives")
            .expect("data frame present");
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["kind"], "wake");
    }

    /// Teardown retires the slot (the body drop runs the synchronous
    /// OnDrop): the retire IS the room-presence offline edge now.
    #[tokio::test]
    async fn drop_retires_the_slot() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-drop".to_string());
        let sse = agent_events(
            State(state.clone()),
            as_tagma(&tagma),
            Path("t-drop".to_string()),
        )
        .await
        .expect("agent stream establishes");
        let body = sse.into_response().into_body().into_data_stream();
        drop(body);
        assert!(
            state
                .read()
                .unwrap()
                .agent_stream_by_tagma(&tagma)
                .is_none(),
            "slot retired at teardown"
        );
    }

    /// The ownership token: a stale connection's cleanup cannot retire a
    /// fresh reconnect's slot, and a second live registration is refused.
    #[tokio::test]
    async fn stale_cleanup_cannot_retire_a_fresh_slot() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-race".to_string());
        let stale_id = Arc::new(());
        let (tx, _rx) = broadcast::channel::<LescheAgentEvent>(8);
        let mut reg = state.write().unwrap();
        assert!(reg.register_agent_stream(&tagma, tx, stale_id.clone()));
        let fresh_id = Arc::new(());
        let (tx2, _rx2) = broadcast::channel::<LescheAgentEvent>(8);
        // The stale channel still has a receiver, so the slot is live: the
        // fresh registration is refused...
        assert!(!reg.register_agent_stream(&tagma, tx2, fresh_id.clone()));
        // ...and retiring with the owning token works exactly once; a second
        // retire (or one with a token that never owned the slot) is a no-op.
        assert!(retire_agent_stream(&mut reg, &tagma, &stale_id));
        assert!(!retire_agent_stream(&mut reg, &tagma, &stale_id));
    }
    /// A retire is the offline edge: the room-presence source for a tagma
    /// is this stream, so removing the slot fires the edge.
    #[tokio::test]
    async fn retire_fires_the_offline_edge() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-sup".to_string());
        let (tx, _rx) = broadcast::channel::<LescheAgentEvent>(8);
        let id = Arc::new(());
        let mut reg = state.write().unwrap();
        assert!(reg.register_agent_stream(&tagma, tx, id.clone()));
        assert!(retire_agent_stream(&mut reg, &tagma, &id), "retire fires");
    }

    /// T-2 pin: `register_agent_stream` reaps a dead slot (its channel has no
    /// receivers left -- the OnDrop cleanup never ran) and replaces it, while
    /// a live slot is refused. Removing the receiver-count arm turns the last
    /// assertion red (a dead slot would 409 the tagma forever).
    #[tokio::test]
    async fn register_reaps_dead_slots_and_refuses_live_ones() {
        let (state, _control) = make_state();
        let tagma = TagmaId::from("t-reap".to_string());
        let mut reg = state.write().unwrap();
        // Live slot: a second registration is refused.
        let (tx, _rx) = broadcast::channel::<LescheAgentEvent>(8);
        assert!(reg.register_agent_stream(&tagma, tx, Arc::new(())));
        let (tx2, _rx2) = broadcast::channel::<LescheAgentEvent>(8);
        assert!(!reg.register_agent_stream(&tagma, tx2, Arc::new(())));
        // Dead slot (its only receiver dropped): reaped and replaced. A
        // second tagma, so the live slot above does not occupy it.
        let dead_tagma = TagmaId::from("t-reap-dead".to_string());
        let (dead_tx, rx) = broadcast::channel::<LescheAgentEvent>(8);
        assert!(reg.register_agent_stream(&dead_tagma, dead_tx, Arc::new(())));
        drop(rx);
        let (fresh_tx, _fresh_rx) = broadcast::channel::<LescheAgentEvent>(8);
        assert!(
            reg.register_agent_stream(&dead_tagma, fresh_tx, Arc::new(())),
            "dead slot reaped at the next connect"
        );
    }

    /// q-2 behavior pin: the agent stream's connect/drop edges fan
    /// RoomMemberOnline/Offline to the room's live human members. Needs a
    /// real membership graph, so it rides the testcontainer `db_state` +
    /// `seed_room` (the DB-less `make_state` cannot observe a fan the room
    /// layer never sees).
    #[tokio::test]
    async fn presence_edges_fan_to_room_members() {
        use crate::test_support::routes::{db_state, seed_room};
        use kallipai_lesche_common::event::LescheEvent;

        async fn recv_edge(
            rx: &mut tokio::sync::broadcast::Receiver<crate::state::SequencedAppEvent>,
        ) -> Option<crate::state::SequencedAppEvent> {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(ev)) => Some(ev),
                _ => None,
            }
        }

        let (state, _control) = db_state().await;
        let owner = UserId::from("owner".to_string());
        let tagma = TagmaId::from("t-edge".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-edge",
            &owner,
            &[&owner],
            &[&tagma],
        )
        .await;
        let mut rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&owner).subscribe()
        };

        // Solo carrier: connect fans online to the room's human member...
        let sse = agent_events(
            State(state.clone()),
            as_tagma(&tagma),
            Path("t-edge".to_string()),
        )
        .await
        .expect("agent stream establishes");
        let ev = recv_edge(&mut rx).await.expect("connect fans online");
        assert!(matches!(
            ev.event,
            LescheEvent::RoomMemberOnline { ref room_id, .. } if room_id.as_ref() == "room-edge"
        ));
        // ...and its teardown fans offline.
        drop(sse.into_response().into_body().into_data_stream());
        let ev = recv_edge(&mut rx)
            .await
            .expect("solo teardown fans offline");
        assert!(matches!(
            ev.event,
            LescheEvent::RoomMemberOffline { ref room_id, .. } if room_id.as_ref() == "room-edge"
        ));
    }
}
