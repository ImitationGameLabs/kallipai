use std::convert::Infallible;
use std::future::Future;
use std::task::Poll;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_core::Stream;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::SignalEvent;
use kallipai_common::protocol::SseEvent;
use kallipai_common::sse::OnDrop;
use kallipai_lesche_common::event::TagmaStatusPayload;
use kallipai_lesche_common::message::{Participant, TagmaReply};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::bus::{AuthoredFrame, SignalFrame, StatusSnapshot, TopicItem, TopicReceiver};

/// One frame on the direct SSE stream — the endpoint's private wire-assembly
/// enum, not a bus item.
/// The variant is the SSE event-name discriminator; the inner value is the
/// `data:` payload. Serialization (variant name + inner JSON) lives in
/// `serialize_direct_frame`.
#[derive(Clone, Debug)]
pub enum DirectFrame {
    /// An authored message forwarded from the projector: an `assistant_content`
    /// event, a `user_message` echo, etc. Carries the sender alongside the
    /// content reply (mirrors the online envelope's `{sender, body}`); already
    /// persisted by the projector.
    Authored {
        sender: Participant,
        reply: TagmaReply,
    },
    /// A runtime signal (busy/idle presence, turn terminals, errors). Ephemeral.
    Signal(SignalEvent),
    /// An aggregate runtime snapshot. Ephemeral.
    Status(TagmaStatusPayload),
    /// The authored source missed frames on this connection: the boundary was
    /// crossed between two delivered events, and the payload names the last
    /// durable row this connection delivered (`None` when it delivered none —
    /// a connection that has seen no row cannot name a resume point). The
    /// reader catches up over `/external/history`; nothing rides this frame.
    Gap { authored_after: Option<i64> },
    /// The tagma is shutting down: this is the last event on the stream.
    /// The reader leaves on it instead of waiting for the body to end, so a
    /// close the server intends is never mistaken for a dropped connection.
    Closing,
}

/// The JSON payload serialized for an `authored` direct-SSE event: the sender
/// paired with the content reply, so the offline frontend (which has no relay
/// envelope) renders the author from one uniform shape.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectAuthoredPayload {
    pub sender: Participant,
    pub reply: TagmaReply,
}

/// The JSON payload serialized for a `gap` direct-SSE event: the last
/// `chat_history.id` this connection delivered, omitted when there is none
/// (the client then falls back to its own rendered window).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectGapPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
}

/// The `chat_history.id` a reply names, if it names a durable row. `0`
/// means "no row recorded" and is not a resume point; replies that carry
/// no id at all (acks, errors) say nothing about the cursor.
fn authored_history_id(reply: &TagmaReply) -> Option<i64> {
    match reply {
        TagmaReply::Event { history_id, .. } | TagmaReply::UserMessage { history_id, .. } => {
            match *history_id {
                0 => None,
                id => Some(id),
            }
        }
        _ => None,
    }
}
/// A shutdown-aware SSE stream over one agent's broadcast channel, with
/// subscribe/unsubscribe transition logging.
///
/// The stream ends for two reasons:
/// - the broadcast sender is dropped (all subscribers gone / agent removed), or
/// - the tagma-wide `shutdown` token fires.
///
/// The shutdown arm is load-bearing: without it, a long-lived SSE connection
/// (e.g. an attached client) keeps the inner `BroadcastStream` open, so hyper's
/// `serve_connection` never completes and `axum::serve(...).with_graceful_shutdown`
/// never returns — Ctrl-C hangs. `take_until(shutdown.cancelled())` ends the
/// stream the instant the tagma-wide token fires, letting graceful shutdown
/// proceed.
///
/// The token passed here is the **tagma-wide** parent, not a per-agent child:
/// SSE shutdown is a tagma lifecycle concern, not an agent lifecycle one.
/// (Compare `bridge_task`, which uses the same parent token for its forced-exit
/// arm for the same reason.)
///
/// # Subscriber-state logging
///
/// Subscribe/unsubscribe is the source of truth for "are this agent's runtime
/// events being observed." We log exactly the `0 <-> 1` transitions of
/// `events_tx.receiver_count()` here — not per runtime event, which would spam
/// on every token delta for any agent (notably subagents) that runs without a
/// subscriber. Attach is logged synchronously in this function; detach is logged
/// from an [`OnDrop`] guard whose closure runs when the stream is dropped.
pub fn sse_stream(
    id: AgentId,
    events_tx: broadcast::Sender<SseEvent>,
    rx: broadcast::Receiver<SseEvent>,
    shutdown: CancellationToken,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 0 -> 1 transition: this subscriber is the first (and, right now, only)
    // one. `subscribe()` already ran at the call site, so the count includes us.
    if events_tx.receiver_count() == 1 {
        info!(id = %id, "SSE subscriber attached");
    }

    // Detach fires from the `OnDrop` guard's `Drop::drop`, which runs before
    // any field is dropped — in particular before the inner broadcast receiver,
    // whose `receiver_count()` decrement is what `should_log_detach` keys off
    // of. Invariant relied on below: the closure observes the receiver still
    // counted. Re-verify if `event_stream` is ever wrapped in a combinator with
    // a custom `Drop` that could drop an inner field early.
    let detach_id = id.clone();
    let detach_tx = events_tx.clone();
    let detach_shutdown = shutdown.clone();

    Sse::new(OnDrop::new(event_stream(rx, shutdown), move || {
        if should_log_detach(&detach_shutdown, &detach_tx) {
            info!(id = %detach_id, "SSE subscriber detached");
        }
    }))
    .keep_alive(KeepAlive::default())
}

/// Whether dropping one subscriber right now should be logged as the
/// "no one is watching" transition.
///
/// - `receiver_count() == 1`: this connection's receiver is still alive in
///   `Drop::drop` (Rust drops fields after the `Drop::drop` body), so a count of
///   1 means we are the last — after the drop completes, none remain. With more
///   than one subscriber, dropping one leaves others watching, so no transition.
/// - `!shutdown.is_cancelled()`: on tagma-wide shutdown every still-attached
///   stream ends via `take_until`, which would otherwise emit a misleading
///   "detached" line per connection (nothing is left to reattach to).
///
/// Best-effort: `receiver_count()` is atomic but a concurrent subscribe/detach
/// can at worst yield one misleading `info`-level line.
fn should_log_detach(
    shutdown: &CancellationToken,
    events_tx: &broadcast::Sender<SseEvent>,
) -> bool {
    !shutdown.is_cancelled() && events_tx.receiver_count() == 1
}

/// Build the shutdown-aware event stream without the SSE/keepalive framing.
///
/// Extracted from [`sse_stream`] so the shutdown contract is directly
/// unit-testable (an `axum::response::Sse` is a response body, not a pollable
/// stream).
///
/// `take_until` lives in `futures_util::stream::StreamExt`; it is applied here
/// via its fully-qualified path so the surrounding sync `filter_map` (from
/// `tokio_stream::StreamExt`, whose closure returns `Option<T>` rather than a
/// future) stays unambiguous.
fn event_stream(
    rx: tokio::sync::broadcast::Receiver<SseEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::StreamExt::take_until(
        BroadcastStream::new(rx),
        shutdown.cancelled_owned(),
    )
    .filter_map(|result| match result {
        Ok(event) => match serde_json::to_string(&event) {
            Ok(data) => Some(Ok(Event::default().data(data))),
            Err(e) => {
                warn!(error = %e, "failed to serialize SSE event, dropping");
                None
            }
        },
        Err(_) => None, // skip lagged messages
    })
}

/// Merge the direct SSE's three bus topics (authored, signal, status) into
/// one [`DirectFrame`] stream. A loss on the authored topic becomes a single
/// `gap` frame naming the last durable row this connection delivered; a
/// loss on either of the other two is dropped, because neither can name a
/// resume point and no frame would say more than the loss itself.
///
/// Cross-topic ordering (settled at this
/// assembly point): intra-topic FIFO holds per source; across sources the
/// interleaving is whatever the three receivers yield. This is an explicit
/// release, not an accident: the frontend demuxes by event name into three
/// queues drained concurrently, so its processing order never depended on
/// cross-type wire order; the online face already carries replies and
/// signals on two independent transports; and `applySignal` /
/// `applyReplyCore` are order-independent reducers. If a future consumer
/// needs cross-type program order, that is a new gate (a merge order tag),
/// not a silent extension of this merge.
pub(crate) fn merge_direct_frames(
    authored: TopicReceiver<AuthoredFrame>,
    signals: TopicReceiver<SignalFrame>,
    status: TopicReceiver<StatusSnapshot>,
) -> impl Stream<Item = DirectFrame> + Send {
    // The authored source is the one that can name a resume point: track the
    // last durable row it delivered and, when the bus reports a loss, say so
    // once with that id. `0` is "no row recorded" and never becomes the
    // cursor (a degraded frame cannot be resumed from).
    let mut last_authored_id: Option<i64> = None;
    let authored = authored.into_stream().map(move |item| match item {
        TopicItem::Data(frame) => {
            if let Some(id) = authored_history_id(&frame.reply) {
                last_authored_id = Some(id);
            }
            DirectFrame::Authored {
                sender: frame.sender,
                reply: frame.reply,
            }
        }
        TopicItem::Missed => DirectFrame::Gap {
            authored_after: last_authored_id,
        },
    });
    let signals = signals.into_stream().filter_map(|item| match item {
        TopicItem::Data(frame) => Some(DirectFrame::Signal(frame.0)),
        TopicItem::Missed => None,
    });
    let status = status.into_stream().filter_map(|item| match item {
        TopicItem::Data(frame) => Some(DirectFrame::Status(frame.0)),
        TopicItem::Missed => None,
    });
    // Three erased sources, interleaved as they yield (no cross-source
    // ordering promise). A loss lands in the topic's lagged counter
    // (`into_stream` keeps the counting leg at the bus core); the authored
    // source expresses it as one `gap` frame, the other two drop it.
    let sources: Vec<std::pin::Pin<Box<dyn Stream<Item = DirectFrame> + Send>>> =
        vec![Box::pin(authored), Box::pin(signals), Box::pin(status)];
    futures_util::stream::select_all(sources)
}

/// A shutdown-aware SSE stream over the direct serving channel. Each
/// [`DirectFrame`] becomes one SSE `Event` whose `event:` name is the frame's
/// channel discriminator (`authored` / `signal` / `status` / `gap` /
/// `closing`) and whose `data:` is the inner value's JSON.
///
/// The shutdown contract is [`sse_stream`]'s plus one frame: when the token
/// fires the stream yields a single `closing` frame and ends, so the reader
/// can tell a close the server intends from a dropped connection; the
/// wrapper that writes it is private to this module.
///
/// `initial` is an optional one-shot frame prepended to the stream — used to
/// push an immediate status snapshot on connect so the chat header renders at
/// once instead of after the next pump tick (~2 s). Serialized through the same
/// `serialize_direct_frame` so the wire shape is identical to a pump-driven
/// status frame; `None` skips it (no leading event).
pub fn direct_sse_stream(
    frames: impl Stream<Item = DirectFrame> + Send + 'static,
    initial: Option<DirectFrame>,
    shutdown: CancellationToken,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    direct_sse_stream_with_keepalive(frames, initial, shutdown, KeepAlive::default())
}

/// [`direct_sse_stream`] with the keepalive cadence injected.
///
/// Production always passes `KeepAlive::default()`. The end-to-end shutdown
/// test needs a cadence short enough to observe inside a test, and confining
/// that knob to a private constructor keeps the serving path from picking a
/// different one.
fn direct_sse_stream_with_keepalive(
    frames: impl Stream<Item = DirectFrame> + Send + 'static,
    initial: Option<DirectFrame>,
    shutdown: CancellationToken,
    keep_alive: KeepAlive,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial_event = initial.and_then(|frame| match serialize_direct_frame(&frame) {
        Ok(event) => Some(Ok(event)),
        Err(e) => {
            warn!(error = %e, "failed to serialize initial direct SSE frame, dropping");
            None
        }
    });
    let live = serialize_direct_frames(close_on_shutdown(frames, shutdown));
    // `stream::iter(Option)` yields the single initial event or nothing, then
    // chains into the live stream. One concrete stream type either way.
    let with_initial = futures_util::stream::iter(initial_event).chain(live);
    Sse::new(with_initial).keep_alive(keep_alive)
}

/// Serialize every frame, dropping the ones that fail: one malformed frame
/// must not end the stream.
fn serialize_direct_frames(
    frames: impl Stream<Item = DirectFrame>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    frames.filter_map(|frame| match serialize_direct_frame(&frame) {
        Ok(event) => Some(Ok(event)),
        Err(e) => {
            warn!(error = %e, "failed to serialize direct SSE frame, dropping");
            None
        }
    })
}

/// Where [`close_on_shutdown`] stands: watching the token, having just
/// yielded the closing frame, or done.
enum CloseState {
    Watching,
    ClosingSent,
    Closed,
}

/// Wrap a direct frame stream so a tagma-wide shutdown becomes a
/// [`DirectFrame::Closing`] on the wire instead of a silent end.
///
/// `take_until` ends a stream the instant the token fires, which is what
/// graceful shutdown needs (see [`sse_stream`]) but leaves the reader unable
/// to tell a planned close from a dropped connection. This wrapper keeps the
/// prompt end and announces it: once the token fires it yields `Closing`, and
/// the very next poll returns `None`. That second poll must not be `Pending` —
/// the keepalive ticker would write a comment after the last event.
fn close_on_shutdown<S>(frames: S, shutdown: CancellationToken) -> impl Stream<Item = DirectFrame>
where
    S: Stream<Item = DirectFrame> + Send,
{
    let mut frames = Box::pin(frames);
    let mut cancelled = Box::pin(shutdown.cancelled_owned());
    let mut state = CloseState::Watching;
    futures_util::stream::poll_fn(move |cx| match state {
        CloseState::Watching => {
            if cancelled.as_mut().poll(cx).is_ready() {
                state = CloseState::ClosingSent;
                return Poll::Ready(Some(DirectFrame::Closing));
            }
            match frames.as_mut().poll_next(cx) {
                Poll::Ready(Some(frame)) => Poll::Ready(Some(frame)),
                Poll::Ready(None) => {
                    state = CloseState::Closed;
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            }
        }
        CloseState::ClosingSent => {
            state = CloseState::Closed;
            Poll::Ready(None)
        }
        CloseState::Closed => Poll::Ready(None),
    })
}

fn serialize_direct_frame(frame: &DirectFrame) -> serde_json::Result<Event> {
    let (name, data) = match frame {
        // The offline SSE carries the sender alongside the reply (the offline
        // path has no relay envelope), so the frontend renders the author from
        // one uniform `{sender, reply}` shape.
        DirectFrame::Authored { sender, reply } => (
            "authored",
            serde_json::to_string(&DirectAuthoredPayload {
                sender: sender.clone(),
                reply: reply.clone(),
            })?,
        ),
        DirectFrame::Signal(event) => ("signal", serde_json::to_string(event)?),
        DirectFrame::Gap { authored_after } => (
            "gap",
            serde_json::to_string(&DirectGapPayload {
                after: *authored_after,
            })?,
        ),
        DirectFrame::Closing => ("closing", "{}".to_string()),
        DirectFrame::Status(payload) => ("status", serde_json::to_string(payload)?),
    };
    Ok(Event::default().event(name).data(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::protocol::SseEvent;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Regression: without the `take_until(shutdown.cancelled())` arm, a
    /// long-lived SSE connection (e.g. an attached client) keeps the inner stream
    /// open and hangs graceful shutdown. The stream must end promptly when the
    /// tagma-wide shutdown token fires — proven here with a tight 100ms bound
    /// (a regression that re-introduces a seconds-long park would be caught).
    #[tokio::test]
    async fn sse_stream_ends_on_shutdown() {
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<SseEvent>(16);
        let rx = events_tx.subscribe();
        let shutdown = CancellationToken::new();

        let stream = event_stream(rx, shutdown.clone());
        tokio::pin!(stream);

        // No events have been sent, so the stream is parked — only the
        // shutdown arm can end it.
        shutdown.cancel();

        let next = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        assert!(
            matches!(next, Ok(None)),
            "SSE stream did not end cleanly after shutdown fired"
        );
    }

    /// A pending event is still delivered when shutdown has not fired.
    ///
    /// Note: `take_until` polls the shutdown future *before* the inner stream,
    /// so a shutdown that is already cancelled by the time the stream is polled
    /// wins and drops the queued event. This test isolates the delivery path by
    /// leaving the token uncancelled.
    #[tokio::test]
    async fn sse_stream_delivers_queued_event() {
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<SseEvent>(16);
        let rx = events_tx.subscribe();
        let shutdown = CancellationToken::new();

        let stream = event_stream(rx, shutdown);
        tokio::pin!(stream);

        // Subscribe first, then publish — broadcast only reaches live receivers.
        events_tx
            .send(SseEvent::Status {
                message: "hi".into(),
            })
            .unwrap();

        // First poll yields the queued event before shutdown ends the stream.
        let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("timed out waiting for first event")
            .expect("stream ended before yielding the event");
        assert!(first.is_ok(), "queued event should have been delivered");
    }

    /// The `OnDrop` guard fires its closure exactly once when the wrapped
    /// stream is dropped — the mechanism that lets `sse_stream` detect a
    /// subscriber detaching.
    #[tokio::test]
    async fn on_drop_runs_closure_when_stream_dropped() {
        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_closure = fired.clone();

        // Inner stream type doesn't matter here — the test never polls it, only
        // asserts the closure runs on drop. Use an empty stream with the Item
        // type `OnDrop` is pinned to.
        let stream = futures_util::stream::empty::<Result<Event, Infallible>>();
        let wrapped = OnDrop::new(stream, move || {
            fired_for_closure.store(true, Ordering::SeqCst);
        });
        drop(wrapped);

        assert!(
            fired.load(Ordering::SeqCst),
            "OnDrop closure must run when the wrapped stream is dropped"
        );
    }

    /// `should_log_detach` is the precise detach-transition predicate: only when
    /// this is the last subscriber AND the tagma is not shutting down.
    #[tokio::test]
    async fn should_log_detach_only_when_last_subscriber_and_not_shutting_down() {
        // Single subscriber: last one -> log.
        let (tx, _rx) = tokio::sync::broadcast::channel::<SseEvent>(4);
        assert!(should_log_detach(&CancellationToken::new(), &tx));

        // Two subscribers: dropping one leaves another -> no log.
        let _second = tx.subscribe();
        assert!(!should_log_detach(&CancellationToken::new(), &tx));

        // Tagma shutting down: suppress even if this is the last subscriber.
        let (tx2, _rx2) = tokio::sync::broadcast::channel::<SseEvent>(4);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(!should_log_detach(&shutdown, &tx2));
    }
    /// The endpoint merge carries all three sources — the authored topic, the
    /// signal topic, and the status topic — as `DirectFrame`s. It closes
    /// once its sources end. Order-agnostic asserts: the merge makes
    /// no cross-source ordering promise.
    #[tokio::test]
    async fn merge_direct_frames_carries_all_three_sources() {
        use kallipai_archeion_common::ids::{ParticipantId, ParticipantKind, UserId};
        let bus = crate::bus::tagma_bus().unwrap();
        let authored = bus.subscribe::<AuthoredFrame>().unwrap();
        let signals = bus.subscribe::<SignalFrame>().unwrap();
        let status = bus.subscribe::<StatusSnapshot>().unwrap();
        bus.publish(AuthoredFrame {
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("u".to_string())),
                kind: ParticipantKind::Human,
                handle: "Alice".into(),
                tagma_id: None,
            },
            reply: TagmaReply::UserMessage {
                history_id: 0,
                text: "hi".into(),
                created_at: None,
                attachment: None,
                quote: None,
            },
        })
        .unwrap();
        bus.publish(SignalFrame(SignalEvent::Busy)).unwrap();
        bus.publish(StatusSnapshot(
            kallipai_lesche_common::event::TagmaStatusPayload {
                root_state: kallipai_common::protocol::AgentState::Idle,
                subagents_total: 0,
                subagents_active: 0,
                token_budget: 50_000,
                token_consumed: 0,
                token_budget_unlimited: false,
            },
        ))
        .unwrap();
        drop(bus); // end the bus-borne sources so the merged stream closes
        let merged = merge_direct_frames(authored, signals, status);
        let frames = tokio::time::timeout(
            Duration::from_millis(500),
            tokio_stream::StreamExt::collect::<Vec<_>>(merged),
        )
        .await
        .expect("merge closes once its sources end");
        let mut authored_n = 0;
        let mut status_n = 0;
        let mut busy = false;
        for frame in &frames {
            match frame {
                DirectFrame::Authored { .. } => authored_n += 1,
                DirectFrame::Signal(SignalEvent::Busy) => busy = true,
                DirectFrame::Signal(_) => {
                    panic!("unexpected frame")
                }
                DirectFrame::Status(_) => status_n += 1,
                DirectFrame::Gap { .. } => {
                    panic!("no loss expected: one published frame per source")
                }
                DirectFrame::Closing => {
                    panic!("only a shutdown emits the closing frame")
                }
            }
        }
        assert_eq!((authored_n, busy, status_n), (1, true, 1));
    }

    /// A three-topic bus local to one test, with ring caps small enough to
    /// force a loss without publishing hundreds of frames.
    fn direct_bus() -> Arc<crate::bus::EventBus> {
        crate::bus::EventBus::builder()
            .topic::<AuthoredFrame>(4)
            .topic::<SignalFrame>(4)
            .topic::<StatusSnapshot>(4)
            .build()
            .expect("three distinct test topics")
            .into()
    }

    /// An authored frame naming a durable row; `0` is the degraded shape a
    /// failed history append produces.
    fn authored_frame(history_id: i64) -> AuthoredFrame {
        use kallipai_archeion_common::ids::{ParticipantId, ParticipantKind, UserId};
        AuthoredFrame {
            sender: Participant {
                id: ParticipantId::for_user(&UserId::from("u".to_string())),
                kind: ParticipantKind::Human,
                handle: "Alice".into(),
                tagma_id: None,
            },
            reply: TagmaReply::Event {
                event: kallipai_common::protocol::AuthoredEvent::AssistantContent {
                    content: format!("row {history_id}"),
                },
                history_id,
                created_at: None,
            },
        }
    }

    /// A reader that was keeping up, then stopped: the loss is reported once,
    /// and it names the last durable row the reader actually received.
    #[tokio::test]
    async fn gap_emitted_once_per_lag() {
        let bus = direct_bus();
        let mut frames = merge_direct_frames(
            bus.subscribe::<AuthoredFrame>().unwrap(),
            bus.subscribe::<SignalFrame>().unwrap(),
            bus.subscribe::<StatusSnapshot>().unwrap(),
        );
        bus.publish(authored_frame(7)).unwrap();
        assert!(matches!(
            frames.next().await,
            Some(DirectFrame::Authored { .. })
        ));

        for id in 8..18 {
            bus.publish(authored_frame(id)).unwrap();
        }

        match frames.next().await {
            Some(DirectFrame::Gap { authored_after }) => assert_eq!(
                authored_after,
                Some(7),
                "the cursor is the last delivered durable row"
            ),
            other => panic!("expected one gap frame, got {other:?}"),
        }
        // One report per loss: what follows is the surviving tail, no gap.
        assert!(matches!(
            frames.next().await,
            Some(DirectFrame::Authored { .. })
        ));
    }

    /// A connection that delivered only degraded frames (`history_id == 0`)
    /// has no resume point to name: the gap is still reported, without an
    /// `after`, so the reader falls back to its own window instead of
    /// replaying from the oldest row.
    #[tokio::test]
    async fn gap_without_anchor_when_nothing_was_delivered() {
        let bus = direct_bus();
        let mut frames = merge_direct_frames(
            bus.subscribe::<AuthoredFrame>().unwrap(),
            bus.subscribe::<SignalFrame>().unwrap(),
            bus.subscribe::<StatusSnapshot>().unwrap(),
        );
        bus.publish(authored_frame(0)).unwrap();
        assert!(matches!(
            frames.next().await,
            Some(DirectFrame::Authored { .. })
        ));

        for _ in 0..10 {
            bus.publish(authored_frame(0)).unwrap();
        }

        match frames.next().await {
            Some(DirectFrame::Gap { authored_after }) => assert_eq!(
                authored_after, None,
                "a degraded frame is not a resume point"
            ),
            other => panic!("expected one gap frame, got {other:?}"),
        }
    }

    /// A loss on the signal topic is dropped, not turned into a gap: only
    /// authored frames can name a resume point, so a gap from this source
    /// would tell the reader nothing it could act on.
    #[tokio::test]
    async fn signal_loss_is_dropped_not_reported() {
        let bus = direct_bus();
        let mut frames = merge_direct_frames(
            bus.subscribe::<AuthoredFrame>().unwrap(),
            bus.subscribe::<SignalFrame>().unwrap(),
            bus.subscribe::<StatusSnapshot>().unwrap(),
        );
        for _ in 0..10 {
            bus.publish(SignalFrame(SignalEvent::Busy)).unwrap();
        }
        // Ten frames into a four-slot ring: the reader lagged, so the next
        // frame is the surviving tail, never a gap.
        match frames.next().await {
            Some(DirectFrame::Signal(_)) => {}
            other => panic!("a signal loss is not a gap, got {other:?}"),
        }
    }

    /// The negative: a reader that keeps up is never told about a gap.
    #[tokio::test]
    async fn no_gap_without_loss() {
        let bus = direct_bus();
        let mut frames = merge_direct_frames(
            bus.subscribe::<AuthoredFrame>().unwrap(),
            bus.subscribe::<SignalFrame>().unwrap(),
            bus.subscribe::<StatusSnapshot>().unwrap(),
        );
        for id in [1, 2] {
            bus.publish(authored_frame(id)).unwrap();
            assert!(matches!(
                frames.next().await,
                Some(DirectFrame::Authored { .. })
            ));
        }
    }

    /// The wire shape, asserted on the bytes the handler writes rather than
    /// on the Rust value: `event: gap` with the cursor in `data`, and an
    /// empty payload when this connection has no resume point.
    #[tokio::test]
    async fn gap_reaches_the_wire() {
        use axum::response::IntoResponse;

        async fn body_of(frame: DirectFrame) -> String {
            let sse = direct_sse_stream(
                futures_util::stream::iter(vec![frame]),
                None,
                CancellationToken::new(),
            );
            let body = sse.into_response().into_body();
            let bytes = axum::body::to_bytes(body, 64 * 1024)
                .await
                .expect("the body is one gap event");
            String::from_utf8(bytes.to_vec()).expect("SSE is UTF-8")
        }

        let with_anchor = body_of(DirectFrame::Gap {
            authored_after: Some(7),
        })
        .await;
        assert!(with_anchor.contains("event: gap"), "got {with_anchor:?}");
        assert!(
            with_anchor.contains("data: {\"after\":7}"),
            "got {with_anchor:?}"
        );

        let without_anchor = body_of(DirectFrame::Gap {
            authored_after: None,
        })
        .await;
        assert!(
            without_anchor.contains("data: {}"),
            "an unknown cursor is omitted, got {without_anchor:?}"
        );
    }

    /// The closing frame is the last thing the stream yields, and the poll
    /// right after it ends the stream. A `Pending` there would let the
    /// keepalive ticker write a comment after the last event.
    #[tokio::test]
    async fn closing_is_the_last_frame() {
        let shutdown = CancellationToken::new();
        let stream = close_on_shutdown(
            futures_util::stream::pending::<DirectFrame>(),
            shutdown.clone(),
        );
        tokio::pin!(stream);

        // Nothing but the shutdown arm can produce a frame here.
        shutdown.cancel();

        let first = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        assert!(
            matches!(first, Ok(Some(DirectFrame::Closing))),
            "the shutdown yields one closing frame"
        );
        let second = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        assert!(
            matches!(second, Ok(None)),
            "the stream ends on the poll after closing"
        );
    }

    /// `closing` on the wire: a named event carrying an empty JSON object.
    /// The object is load-bearing -- the shared frontend parser drops frames
    /// that carry no `data:` line, so a bare `event: closing` would never
    /// reach a client.
    #[tokio::test]
    async fn closing_reaches_the_wire() {
        use axum::response::IntoResponse;

        let shutdown = CancellationToken::new();
        let sse = direct_sse_stream(
            futures_util::stream::pending::<DirectFrame>(),
            None,
            shutdown.clone(),
        );
        shutdown.cancel();
        let bytes = axum::body::to_bytes(sse.into_response().into_body(), 64 * 1024)
            .await
            .expect("the closing body");
        let body = String::from_utf8(bytes.to_vec()).expect("SSE is UTF-8");

        assert!(body.contains("event: closing"), "got {body:?}");
        assert!(body.contains("data: {}"), "got {body:?}");
        let closing = body
            .find("event: closing")
            .expect("one closing event on the wire");
        assert!(
            !body[closing + "event: closing".len()..].contains("event: "),
            "no event follows closing, got {body:?}"
        );
    }

    /// A real graceful shutdown announces itself: the body's last event is
    /// `closing`, and the connection ends right after it. Ten rounds, because
    /// one pass could see the right order by luck.
    #[tokio::test]
    async fn shutdown_sends_closing_before_ending_the_stream() {
        for round in 0..10 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let shutdown = CancellationToken::new();
            let route_token = shutdown.clone();
            let app = axum::Router::new().route(
                "/events",
                axum::routing::get(move || {
                    let token = route_token.clone();
                    async move {
                        direct_sse_stream_with_keepalive(
                            futures_util::stream::pending::<DirectFrame>(),
                            Some(DirectFrame::Status(TagmaStatusPayload {
                                root_state: kallipai_common::protocol::AgentState::Idle,
                                subagents_total: 0,
                                subagents_active: 0,
                                token_budget: 0,
                                token_consumed: 0,
                                token_budget_unlimited: false,
                            })),
                            token,
                            KeepAlive::new().interval(Duration::from_millis(50)),
                        )
                    }
                }),
            );
            let serving = tokio::spawn({
                let token = shutdown.clone();
                async move {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async move { token.cancelled().await })
                        .await
                        .expect("the test server serves");
                }
            });

            let response = reqwest::Client::new()
                .get(format!("http://{addr}/events"))
                .send()
                .await
                .expect("the stream connects");
            let body = response.bytes_stream();
            tokio::pin!(body);

            // Synchronise on the connect-time snapshot: cancelling before it
            // lands would race the closing frame against the first event.
            let mut seen = String::new();
            while !seen.contains("event: status") {
                let chunk = body.next().await.expect("a chunk before cancellation");
                seen.push_str(&String::from_utf8_lossy(&chunk.expect("a body chunk")));
            }
            shutdown.cancel();
            while let Some(chunk) = body.next().await {
                seen.push_str(&String::from_utf8_lossy(&chunk.expect("a body chunk")));
            }
            tokio::time::timeout(Duration::from_secs(5), serving)
                .await
                .unwrap_or_else(|_| {
                    panic!("round {round}: the server did not return after the stream ended")
                })
                .expect("the server task joins");

            let closing = seen
                .find("event: closing")
                .unwrap_or_else(|| panic!("round {round}: no closing event in {seen:?}"));
            assert!(
                !seen[closing + "event: closing".len()..].contains("event: "),
                "round {round}: closing is the last event, got {seen:?}"
            );
        }
    }
}
