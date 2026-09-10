//! The multiplexed app event stream (`GET /v1/me/events`). One connection per
//! user carries envelope deliveries for all of their conversations plus presence
//! transitions: `TagmaOnline`/`TagmaOffline` for the user's own tagmata, and
//! `RoomMemberOnline`/`RoomMemberOffline` for peers in the user's rooms (fanned
//! by [`crate::room_presence`] as participants connect/disconnect). On open, the
//! stream emits the current presence snapshot for the user's online tagmata
//! plus each tagma's latest cached status snapshot;
//! room-member presence arrives live and is resynced by the roster's `online`
//! field on each roster fetch.
//!
//! Wire framing: the stream opens with a marker frame (`event: stream`,
//! `data: {"epoch": <u32>, "next_seq": <u64>}`, emitted before the initial
//! flush; `next_seq` is the seq of the first frame allocated after the marker
//! capture; frames allocated earlier may still arrive first (pre-adoption)), and
//! every payload frame carries an SSE `id` of the form `<epoch>:<seq>`. Payload
//! bytes are untouched — clients that ignore `id:` and unknown event names
//! keep working unchanged.
//!
//! If a slow client falls behind the broadcast capacity, the channel drops
//! frames server-side (logged at `warn`); the client detects the loss itself
//! via the id jump and resyncs.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::event::LescheEvent;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::auth::{AuthPrincipal, require_user};
use crate::sse::{BoxEventStream, OnDrop};
use crate::state::SharedConvState;
use kallip_archeion_common::ids::ParticipantId;

pub fn router() -> Router<SharedConvState> {
    Router::new().route("/me/events", get(me_events))
}

async fn me_events(
    State(state): State<SharedConvState>,
    AuthPrincipal(principal): AuthPrincipal,
) -> Result<Sse<axum::response::sse::KeepAliveStream<OnDrop>>, ApiError> {
    let user_id = require_user(&principal)?.clone();
    // Capture the runtime handle so the synchronous `OnDrop` cleanup can spawn
    // the presence fan-out without `tokio::spawn`'s implicit `Handle::current()`
    // panic (Drop may run off-runtime during body teardown).
    let handle = tokio::runtime::Handle::try_current().ok();
    let (stream, was_first) = {
        let mut reg = state.write()?;
        // The 0 -> 1 edge: announce room presence only on the user's FIRST live
        // app stream, so a second tab re-opening does not re-announce. Race-free
        // under this write lock; relies on `me_events` being the sole
        // `app_streams` creator -- a future caller
        // outside `me_events` would need to revisit.
        let was_first = !reg.has_app_stream(&user_id);
        let stream = reg.open_app_stream(&user_id);
        (stream, was_first)
    };
    // Announce room-member presence to peers (best-effort, off the request path),
    // spawned AFTER the write guard releases -- matching the tunnel connect path
    // and keeping every fan-out site uniform: no spawn under a registry lock.
    if was_first && let Some(h) = &handle {
        let st = state.clone();
        let who = ParticipantId::for_user(&user_id);
        h.spawn(async move {
            crate::room_presence::fan_member_presence(&st, &who, true).await;
        });
    }
    // Snapshot-then-live via the shared primitive: `stream.subscribe()` runs
    // before the presence read, so a tunnel connecting concurrently may be
    // delivered twice (once in the initial flush, once as its own live
    // TagmaOnline); clients MUST treat presence as an idempotent set, not
    // assume exactly-once. No online tagma is missed.
    let (rx, initial) = kallip_common::sse::open_snapshot_stream(
        || stream.subscribe(),
        || async {
            let Ok(reg) = state.read() else {
                // A poisoned registry means a writer panicked while
                // holding the lock between this connect's registry write
                // and this read. Poisoning is permanent within the process
                // (cleared only by restart); the connect itself is healthy,
                // so keep the channel open with an empty initial flush
                // rather than 500ing into a reconnect loop -- deliveries
                // that do not need this lock still land.
                tracing::warn!("app SSE initial flush skipped: registry poisoned");
                return Vec::new();
            };
            let mut initial = Vec::new();
            for entry in reg.presence.values() {
                if entry.owner != user_id {
                    continue;
                }
                initial.push(LescheEvent::TagmaOnline {
                    tagma_id: entry.tagma_id.clone(),
                });
                // Initial status flush: late-connecting clients get the latest
                // cached snapshot immediately instead of waiting for the next
                // pump heartbeat. Idempotent with the live fan (status.set).
                if let Some(st) = &entry.latest_status {
                    initial.push(LescheEvent::TagmaStatus {
                        tagma_id: entry.tagma_id.clone(),
                        root_state: st.root_state,
                        subagents_total: st.subagents_total,
                        subagents_active: st.subagents_active,
                        token_budget: st.token_budget,
                        token_consumed: st.token_consumed,
                    });
                }
            }
            initial
        },
    )
    .await;
    // Capture the stream marker before the flush, under the same send_lock
    // the deliveries use: `next_seq` is the seq of the first frame allocated
    // after this point, so the snapshot flush continues at it with no false gap.
    let (epoch, next_seq) = stream.capture_marker();
    // Flush the snapshot through the shared deliver helper so initial frames
    // carry seqs exactly like live ones (channel order preserved).
    for ev in initial {
        let _ = stream.deliver(ev);
    }
    // Wire framing: the marker opens the stream; every payload frame
    // carries `id: "<epoch>:<seq>"`. Payload bytes stay pure LescheEvent.
    let marker = Event::default()
        .event("stream")
        .json_data(serde_json::json!({ "epoch": epoch, "next_seq": next_seq }))
        .expect("marker serializes");
    let frames: BoxEventStream = Box::pin(
        tokio_stream::iter([Ok::<Event, std::convert::Infallible>(marker)]).chain(
            BroadcastStream::new(rx)
                .filter_map(|r| match r {
                    Ok(frame) => Some(frame),
                    Err(BroadcastStreamRecvError::Lagged(n)) => {
                        tracing::warn!(
                            lag = n,
                            "app SSE lagged; client detects the drop via the id jump"
                        );
                        None
                    }
                })
                .map(|frame| {
                    Ok::<Event, std::convert::Infallible>(
                        Event::default()
                            .id(format!("{}:{}", frame.epoch, frame.seq))
                            .json_data(frame.event)
                            .expect("event serializes"),
                    )
                }),
        ),
    );

    // The handle shares the stream's channel; `receiver_count()` includes our
    // own subscribed rx (still alive during this closure), so `== 1` == "last one".
    let cleanup_state = state.clone();
    let cleanup_user = user_id.clone();
    let cleanup_stream = stream.clone();
    let cleanup_handle = handle.clone();
    let cleaned = OnDrop::new(frames, move || {
        let removed = {
            let Ok(mut reg) = cleanup_state.write() else {
                return;
            };
            reg.remove_app_stream_if_last(&cleanup_user, &cleanup_stream)
        };
        // On the 1 -> 0 edge, fan room-member offline to peers. The guard is
        // dropped above; the (async) fan-out is spawned off-thread.
        if removed {
            if let Some(h) = cleanup_handle.as_ref() {
                let st = cleanup_state.clone();
                let who = ParticipantId::for_user(&cleanup_user);
                h.spawn(async move {
                    crate::room_presence::fan_member_presence(&st, &who, false).await;
                });
            } else {
                tracing::warn!("offline-presence fan skipped: no runtime at drop");
            }
        }
    });
    // `: ping` comment every 5s so quiet periods cannot let an idle
    // timeout reap the stream mid-read (parser ignores comments).
    Ok(Sse::new(cleaned).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("ping"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use axum::body::BodyDataStream;
    use kallip_archeion_common::ids::{TagmaId, UserId};
    use kallip_archeion_common::principal::Principal;
    use kallip_common::protocol::AgentState;
    use kallip_lesche_common::event::TagmaStatusPayload;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    async fn read_frame(stream: &mut BodyDataStream) -> String {
        let frame = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio_stream::StreamExt::next(stream),
        )
        .await
        .expect("frame within timeout")
        .expect("stream live")
        .expect("frame bytes");
        String::from_utf8(frame.to_vec()).expect("utf8 frame")
    }

    /// Pool-item leg ("me_events connect flush read path"): a connect must
    /// immediately flush TagmaOnline plus the latest cached status snapshot
    /// for each of the user's online tagmata, ahead of any live traffic.
    #[tokio::test]
    async fn me_events_connect_flushes_online_and_latest_status() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_tunnel_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        {
            use kallip_archeion_common::ids::ParticipantId;
            let mut reg = state.write().unwrap();
            let pid = ParticipantId::for_tagma(&tagma);
            reg.presence.get_mut(&pid).unwrap().latest_status = Some(TagmaStatusPayload {
                root_state: AgentState::Busy,
                subagents_total: 3,
                subagents_active: 2,
                token_budget: 50_000,
                token_consumed: 100,
                token_budget_unlimited: false,
            });
        }

        let sse = me_events(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner.clone())),
        )
        .await
        .expect("owner opens the app stream");
        let mut stream = axum::response::IntoResponse::into_response(sse)
            .into_body()
            .into_data_stream();

        // Wire framing: the marker opens the stream and the flush continues
        // at its next_seq (the enforcement anchor: gap-free marker→flush).
        let marker = read_frame(&mut stream).await;
        let (epoch, next_seq) = parse_marker(&marker);
        let online = read_frame(&mut stream).await;
        assert!(
            online.contains("\"type\":\"tagma_online\"") && online.contains("tagma-1"),
            "first flush frame must be the online flush: {online:?}"
        );
        assert_eq!(
            frame_id(&online),
            format!("{epoch}:{next_seq}"),
            "flush ids continue at the marker's next_seq"
        );
        let status = read_frame(&mut stream).await;
        assert!(
            status.contains("\"type\":\"tagma_status\"")
                && status.contains("\"token_consumed\":100"),
            "second flush frame must be the cached status snapshot: {status:?}"
        );
        assert_eq!(frame_id(&status), format!("{epoch}:{}", next_seq + 1));
        // Payload purity: the cursor rides the id field, never the data bytes.
        assert!(!online.contains("\"epoch\"") && !online.contains("\"seq\""));
    }

    /// Flush-ordering leg: a live event pushed into the app-stream channel
    /// after the handler returned (subscription open, snapshot flushed) is
    /// not lost — it arrives after the initial frames, carrying the newer
    /// snapshot. With the primitive's ordering test this pins "the snapshot
    /// precedes no live event, and misses none".
    #[tokio::test]
    async fn me_events_live_event_after_connect_is_not_lost() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_tunnel_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        {
            use kallip_archeion_common::ids::ParticipantId;
            let mut reg = state.write().unwrap();
            let pid = ParticipantId::for_tagma(&tagma);
            reg.presence.get_mut(&pid).unwrap().latest_status = Some(TagmaStatusPayload {
                root_state: AgentState::Busy,
                subagents_total: 1,
                subagents_active: 0,
                token_budget: 50_000,
                token_consumed: 100,
                token_budget_unlimited: false,
            });
        }

        let sse = me_events(
            State(state.clone()),
            AuthPrincipal(Principal::User(owner.clone())),
        )
        .await
        .expect("owner opens the app stream");
        let mut stream = axum::response::IntoResponse::into_response(sse)
            .into_body()
            .into_data_stream();

        // Live publish after connect: the handler's receiver is already
        // subscribed, so this must reach the stream after the flush frames.
        state
            .read()
            .unwrap()
            .app_stream(&owner)
            .unwrap()
            .deliver(LescheEvent::TagmaStatus {
                tagma_id: tagma.clone(),
                root_state: AgentState::Busy,
                subagents_total: 1,
                subagents_active: 1,
                token_budget: 50_000,
                token_consumed: 200,
            })
            .expect("live send lands");

        let marker = read_frame(&mut stream).await;
        let (epoch, next_seq) = parse_marker(&marker);
        let online = read_frame(&mut stream).await;
        assert!(online.contains("\"type\":\"tagma_online\""), "{online:?}");
        assert_eq!(frame_id(&online), format!("{epoch}:{next_seq}"));
        let flushed = read_frame(&mut stream).await;
        assert!(
            flushed.contains("\"type\":\"tagma_status\"")
                && flushed.contains("\"token_consumed\":100"),
            "the flush carries the connect-time snapshot: {flushed:?}"
        );
        assert_eq!(frame_id(&flushed), format!("{epoch}:{}", next_seq + 1));
        let live = read_frame(&mut stream).await;
        assert!(
            live.contains("\"type\":\"tagma_status\"") && live.contains("\"token_consumed\":200"),
            "the post-connect live frame is not lost: {live:?}"
        );
        assert_eq!(
            frame_id(&live),
            format!("{epoch}:{}", next_seq + 2),
            "live ids continue the cursor: the frame is neither lost nor gapped"
        );
    }

    /// Extract the SSE `id:` value from a raw frame (the `<epoch>:<seq>` cursor).
    fn frame_id(frame: &str) -> &str {
        frame
            .lines()
            .find_map(|line| line.strip_prefix("id:"))
            .map(str::trim)
            .expect("frame carries an SSE id")
    }

    /// Parse the open-stream marker's data payload: `{"epoch": e, "next_seq": n}`.
    fn parse_marker(frame: &str) -> (u32, u64) {
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .expect("marker carries data");
        let value: serde_json::Value = serde_json::from_str(data).expect("marker json");
        (
            value["epoch"].as_u64().expect("epoch") as u32,
            value["next_seq"].as_u64().expect("next_seq"),
        )
    }

    /// Made observable: two tabs on one user share the one
    /// stream, so a frame fanned once carries the identical `(epoch, seq)`
    /// on both tabs.
    #[tokio::test]
    async fn two_tabs_observe_identical_epoch_and_seq() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_tunnel_tx, _id) = seed_presence(&state, &tagma, owner.clone());

        let mut tab1 = open_tab(state.clone(), owner.clone()).await;
        let mut tab2 = open_tab(state.clone(), owner.clone()).await;

        // Deterministic drain: each connect flushes exactly one TagmaOnline
        // (no cached status in this fixture). Tab2's flush echo is a shared
        // channel frame, so tab1 observes it too — tab1 drains two flush
        // frames, tab2 drains its own.
        let _marker1 = read_frame(&mut tab1).await;
        let _marker2 = read_frame(&mut tab2).await;
        let first_flush = read_frame(&mut tab1).await;
        assert!(
            first_flush.contains("\"type\":\"tagma_online\""),
            "{first_flush:?}"
        );
        let echo_on_tab1 = read_frame(&mut tab1).await;
        assert!(
            echo_on_tab1.contains("\"type\":\"tagma_online\""),
            "{echo_on_tab1:?}"
        );
        let flush_on_tab2 = read_frame(&mut tab2).await;
        assert!(
            flush_on_tab2.contains("\"type\":\"tagma_online\""),
            "{flush_on_tab2:?}"
        );

        // One live fan, identical cursor framing on both tabs.
        state
            .read()
            .unwrap()
            .app_stream(&owner)
            .unwrap()
            .deliver(LescheEvent::TagmaOnline {
                tagma_id: tagma.clone(),
            })
            .expect("delivered");
        let id1 = frame_id(&read_frame(&mut tab1).await).to_string();
        let id2 = frame_id(&read_frame(&mut tab2).await).to_string();
        assert_eq!(id1, id2, "both tabs must observe the same (epoch, seq)");
    }

    /// Open a fresh me_events connection and return its raw frame stream.
    async fn open_tab(state: SharedConvState, owner: UserId) -> BodyDataStream {
        let sse = me_events(State(state), AuthPrincipal(Principal::User(owner)))
            .await
            .expect("tab opens the app stream");
        axum::response::IntoResponse::into_response(sse)
            .into_body()
            .into_data_stream()
    }

    /// Channel recreation: the last tab leaving (the 1 -> 0 edge) removes the
    /// stream, so a reconnect gets a fresh channel with a bumped epoch and a
    /// zeroed cursor — the wire signal for clients to full-resync.
    #[tokio::test]
    async fn reopen_after_the_last_tab_leaves_bumps_the_epoch() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        let (_tunnel_tx, _id) = seed_presence(&state, &tagma, owner.clone());

        let mut stream = open_tab(state.clone(), owner.clone()).await;
        let (epoch1, _) = parse_marker(&read_frame(&mut stream).await);
        let _online = read_frame(&mut stream).await;
        drop(stream); // OnDrop cleanup: the 1 -> 0 edge removes the entry.

        let mut stream = open_tab(state, owner).await;
        let (epoch2, next_seq2) = parse_marker(&read_frame(&mut stream).await);
        assert_ne!(epoch2, epoch1, "a recreated stream must change the epoch");
        assert_eq!(next_seq2, 0, "a fresh channel starts its cursor at zero");
    }

    /// Fan burst through the shared deliver helper: allocation and send share
    /// one lock, so client-observed ids stay dense and ascending no matter how
    /// the bursts interleave (lock-free allocation would manufacture gaps).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn interleaved_fan_burst_keeps_ids_dense_and_ascending() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");

        let mut stream = open_tab(state.clone(), owner.clone()).await;
        let (epoch, next_seq) = parse_marker(&read_frame(&mut stream).await);

        // Four concurrent bursts yielding between deliveries (true
        // interleaving on the test runtime), all through the one helper.
        let burst = |stream: std::sync::Arc<crate::state::AppStream>, tagma: TagmaId| async move {
            for _ in 0..25 {
                stream
                    .deliver(LescheEvent::TagmaOnline {
                        tagma_id: tagma.clone(),
                    })
                    .expect("delivered");
                tokio::task::yield_now().await;
            }
        };
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let stream = state.read().unwrap().app_stream(&owner).unwrap();
                let tagma = TagmaId::from("burst".to_string());
                tokio::spawn(burst(stream, tagma))
            })
            .collect();
        for handle in handles {
            handle.await.expect("burst task");
        }

        for expected in next_seq..next_seq + 100 {
            let frame = read_frame(&mut stream).await;
            assert_eq!(
                frame_id(&frame),
                format!("{epoch}:{expected}"),
                "ids must be dense and ascending across the burst"
            );
        }
    }
}
