//! The in-process typed topic bus: a fixed registry of tokio broadcast
//! channels keyed by event type, published and subscribed through generic
//! typed APIs.

//! Shape (mid-envelope dispatch):
//! every topic is registered at construction —
//! `build` rejects a duplicate event type — so `publish` pays one index
//! lookup and one downcast per call (not per receiver). The registry itself
//! is the idempotence guard; call sites never check. Topics carry bounded
//! capacity with lag-drop: loss is
//! detectable (per-topic lagged counter + loss range) and receivers self-heal via
//! history re-pull / snapshot cadence.

//! This core will never grow (permanent): no middleware, no
//! priorities, no cross-topic routing, no persistence, no observer-style
//! callbacks — the bus is the buffered, pulled half of the event system
//! only. New topics are one `Event` impl + one line at the registration
//! site ([`tagma_bus`]); new behavior is a consumer, not a core feature.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_core::Stream;
use kallip_common::protocol::SignalEvent;
use kallip_lesche_common::event::TagmaStatusPayload;
use kallip_lesche_common::message::{Participant, TagmaReply};
use kallip_lesche_common::projection::ProjectionSnapshot as ProjectionSnapshotPayload;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// Sealed vocabulary of bus topics. Implementations live in this module
/// only: an `Event` type's [`TypeId`] is its topic key, `TOPIC` is its
/// diagnostic name (metrics snapshots, registry errors), and `Clone` is
/// required because broadcast fans one event out per receiver.
pub(crate) trait Event: Any + Clone + Send + Sync {
    const TOPIC: &'static str;
}

/// One authored message: the sender paired with the persisted, stamped
/// reply. The projector — the sole writer — publishes; the relay envelope
/// pump and the local SSE assembler subscribe.
#[derive(Clone, Debug)]
pub(crate) struct AuthoredFrame {
    pub(crate) sender: Participant,
    pub(crate) reply: TagmaReply,
}

impl Event for AuthoredFrame {
    const TOPIC: &'static str = "authored_frame";
}

/// A runtime signal (busy/idle presence, turn terminals, errors): ephemeral,
/// never persisted, carries no sender. Consumers: the relay signal path, the
/// projection pump wake, the local SSE assembler.
#[derive(Clone, Debug)]
pub(crate) struct SignalFrame(pub(crate) SignalEvent);

impl Event for SignalFrame {
    const TOPIC: &'static str = "signal_frame";
}
/// An aggregate runtime status snapshot (root state + subagent counts + token
/// budget): ephemeral, self-healing by construction — a lost frame is refreshed
/// by the next snapshot (2 s cadence / watch wake), so lag costs staleness only.
/// Producers: the snapshot pump drivers (relay + direct); consumers:
/// the local SSE assembler and the upstream flusher (the wire writer).
#[derive(Clone, Debug)]
pub(crate) struct StatusSnapshot(pub(crate) TagmaStatusPayload);

impl Event for StatusSnapshot {
    const TOPIC: &'static str = "status_snapshot";
}

/// A full manage-plane projection (roster + status + work schedule): the
/// lesche's PUT-fed read model, with the same self-healing economics as
/// [`StatusSnapshot`]. Producer: the projection pump driver; consumers: the
/// upstream flusher (the wire writer) and nothing else — a no-subscriber
/// publish is benign by bus contract.
#[derive(Clone, Debug)]
pub(crate) struct ProjectionSnapshot(pub(crate) ProjectionSnapshotPayload);

impl Event for ProjectionSnapshot {
    const TOPIC: &'static str = "projection_snapshot";
}

/// A task-ledger mutation worth waking the task's people for: published by
/// the task routes after every write verb; consumed by the task watcher,
/// which drops a wake hint into the prompt queue of every agent whose role
/// appears in the task's people set (assignee, confirmers, creator). Live-only
/// economics: a lost frame costs one missed hint, and the next verb on the
/// task re-announces the current state.
#[derive(Clone, Debug)]
pub(crate) struct TaskChanged {
    pub(crate) task_id: i64,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) verb: String,
    pub(crate) creator: Option<String>,
    pub(crate) assignee: Option<String>,
    pub(crate) confirmers: Vec<String>,
}

impl Event for TaskChanged {
    const TOPIC: &'static str = "task_changed";
}

/// Per-topic capacities. Bounded memory with lag-drop is the
/// written contract: a receiver that falls `capacity` behind loses frames and
/// must self-heal (history re-pull / snapshot cadence). Snapshot topics carry
/// 16 slots: a snapshot's self-heal is the NEXT snapshot, so capacity beyond
/// a handful is dead buffer.
const AUTHORED_CAPACITY: usize = 64;
const SIGNAL_CAPACITY: usize = 256;
const STATUS_CAPACITY: usize = 16;
const PROJECTION_CAPACITY: usize = 16;

const TASK_CAPACITY: usize = 64;
/// The tagma's topic registry: the single registration site —
/// one site, one shape.
pub(crate) fn tagma_bus() -> Result<EventBus, RegistryError> {
    EventBus::builder()
        .topic::<AuthoredFrame>(AUTHORED_CAPACITY)
        .topic::<SignalFrame>(SIGNAL_CAPACITY)
        .topic::<StatusSnapshot>(STATUS_CAPACITY)
        .topic::<ProjectionSnapshot>(PROJECTION_CAPACITY)
        .topic::<TaskChanged>(TASK_CAPACITY)
        .build()
}

/// Bus-internal channel payload: the per-topic sequence travels with the
/// event inside the erased slot. Never visible to consumers
/// — `TopicReceiver` unwraps it, so call-site and stream signatures are
/// unchanged.
#[derive(Clone)]
struct Sequenced<E> {
    seq: u64,
    event: E,
}
/// Per-topic counters — the single instrumentation site (the
/// measured-upgrade path reads these).
#[derive(Debug, Default)]
struct TopicMetrics {
    /// Events accepted onto the topic (no-subscriber sends count too).
    publishes: AtomicU64,
    /// Events reported lost by any receiver's `Lagged` (cumulative
    /// across receivers; a receiver that falls `n` behind adds `n`).
    lagged: AtomicU64,
    /// Next per-topic sequence number. Allocated under the slot's
    /// ordering lock immediately before the broadcast send, so
    /// in-channel order == allocation order.
    next_seq: AtomicU64,
    /// Most recently observed loss range, inclusive both ends
    /// (diagnostics; last writer wins across receivers).
    last_gap: Mutex<Option<(u64, u64)>>,
}

/// Point-in-time copy of one topic's counters (diagnostics reader).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TopicStats {
    pub(crate) topic: &'static str,
    pub(crate) publishes: u64,
    pub(crate) lagged: u64,
    pub(crate) last_gap: Option<(u64, u64)>,
}

/// Erased per-topic state: the typed sender behind [`Any`], the topic's
/// diagnostic name, its metrics cell, and its send-ordering lock.
struct TopicSlot {
    tx: Box<dyn Any + Send + Sync>,
    name: &'static str,
    metrics: Arc<TopicMetrics>,
    /// Serializes seq allocation and broadcast send per topic:
    /// an atomic alone would admit a send-lock reorder — a
    /// receiver trusting `next = last+1` would raise a false gap.
    /// Producer census (tree-verified): AuthoredFrame
    /// publishes from 3 sites (record_outbound, record_inbound,
    /// run_event_pump) and StatusSnapshot from 2 (the relay status
    /// pump and the direct status driver), so those topics are
    /// multi-site today and the lock carries real contention now —
    /// this is load-bearing, not future-proofing.
    send_order: Mutex<()>,
}

/// Registry construction error: a duplicate event type means two topics
/// share one TypeId key — a programming error at the single registration
/// site, not a runtime condition.
#[derive(Debug)]
pub(crate) enum RegistryError {
    Duplicate { type_name: &'static str },
}

/// Publish errors. `Unregistered` is the runtime no-op path (logged and
/// dropped at the call site — the hot path never panics). `SlotMismatch`
/// would mean the erased slot's type diverged from the index key, i.e. a
/// broken build of the registry itself.
#[derive(Debug)]
pub(crate) enum PublishError {
    Unregistered { type_name: &'static str },
    SlotMismatch { type_name: &'static str },
}

/// Subscribe errors: the topic was never registered, or the slot's erased
/// type diverged from the index key (a broken registry build).
#[derive(Debug)]
pub(crate) enum SubscribeError {
    Unregistered { type_name: &'static str },
    SlotMismatch { type_name: &'static str },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Duplicate { type_name } => {
                write!(f, "duplicate event type in the topic registry: {type_name}")
            }
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::Unregistered { type_name } => {
                write!(f, "publish on an unregistered topic: {type_name}")
            }
            PublishError::SlotMismatch { type_name } => {
                write!(f, "topic slot type mismatch: {type_name}")
            }
        }
    }
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscribeError::Unregistered { type_name } => {
                write!(f, "subscribe on an unregistered topic: {type_name}")
            }
            SubscribeError::SlotMismatch { type_name } => {
                write!(f, "topic slot type mismatch: {type_name}")
            }
        }
    }
}
/// The typed topic bus. Immutable after `build`; held by value on
/// [`crate::state::AppState`], so every holder of the state sees it.
pub(crate) struct EventBus {
    slots: Vec<TopicSlot>,
    index: HashMap<TypeId, usize>,
}

impl EventBus {
    /// Registration builder: topics accumulate here; `build` rejects
    /// duplicates.
    pub(crate) fn builder() -> EventBusBuilder {
        EventBusBuilder::default()
    }

    /// Publish one event to its topic: one index lookup and one downcast
    /// per call — never per receiver (the mid-envelope dispatch form).
    /// Sending with no receivers is benign (live-only topics need no
    /// durable echo; the authored half is durable via the projector's
    /// persist-once), so only registry-level faults are errors. The hot
    /// path never panics: an error means log-and-drop at the call site.
    /// Each accepted event takes the topic's next sequence number under
    /// the per-topic ordering lock: in-channel order == allocation order,
    /// the precondition every downstream gap rule relies on.
    /// Returns the allocated seq; the per-site debug traces of
    /// it at the static call sites are what attribute a lost range to
    /// the site that dropped it.
    pub(crate) fn publish<E: Event>(&self, event: E) -> Result<u64, PublishError> {
        let type_name = std::any::type_name::<E>();
        let slot = self
            .slot::<E>()
            .ok_or(PublishError::Unregistered { type_name })?;
        let tx = slot
            .tx
            .downcast_ref::<broadcast::Sender<Sequenced<E>>>()
            .ok_or(PublishError::SlotMismatch { type_name })?;
        // Seq consumption boundary:
        // the `?` arms above exit BEFORE the allocation point and consume
        // no seq; a no-subscriber send error below is past it — the seq is
        // spent, which is the benign by-bus-contract case.
        let _order = slot.send_order.lock().unwrap_or_else(|e| e.into_inner());
        let seq = slot.metrics.next_seq.fetch_add(1, Ordering::Relaxed);
        let _ = tx.send(Sequenced { seq, event });
        slot.metrics.publishes.fetch_add(1, Ordering::Relaxed);
        Ok(seq)
    }

    /// Subscribe to a topic. The returned receiver reports lag into the
    /// topic's metrics transparently; adapters consume the counting stream
    /// via [`TopicReceiver::into_stream`] (the SSE assembly does).
    pub(crate) fn subscribe<E: Event>(&self) -> Result<TopicReceiver<E>, SubscribeError> {
        let type_name = std::any::type_name::<E>();
        let slot = self
            .slot::<E>()
            .ok_or(SubscribeError::Unregistered { type_name })?;
        let tx = slot
            .tx
            .downcast_ref::<broadcast::Sender<Sequenced<E>>>()
            .ok_or(SubscribeError::SlotMismatch { type_name })?;
        Ok(TopicReceiver {
            rx: tx.subscribe(),
            metrics: Arc::clone(&slot.metrics),
            name: slot.name,
            last_delivered: None,
        })
    }

    /// Point-in-time snapshot of every topic's counters.
    pub(crate) fn stats(&self) -> Vec<TopicStats> {
        self.slots
            .iter()
            .map(|slot| TopicStats {
                topic: slot.name,
                publishes: slot.metrics.publishes.load(Ordering::Relaxed),
                lagged: slot.metrics.lagged.load(Ordering::Relaxed),
                last_gap: *slot
                    .metrics
                    .last_gap
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            })
            .collect()
    }

    fn slot<E: Event>(&self) -> Option<&TopicSlot> {
        self.index.get(&TypeId::of::<E>()).map(|&i| &self.slots[i])
    }
}

/// Accumulates topic registrations; `build` rejects a duplicate event type
/// (two topics sharing one TypeId key is a programming error at this
/// single site, not a runtime condition).
#[derive(Default)]
pub(crate) struct EventBusBuilder {
    slots: Vec<TopicSlot>,
    keys: Vec<(TypeId, &'static str)>,
}

impl EventBusBuilder {
    /// Register one topic: the event type (its TypeId is the topic key)
    /// with its broadcast capacity (bounded memory, lag-drop).
    pub(crate) fn topic<E: Event>(mut self, capacity: usize) -> Self {
        self.keys.push((TypeId::of::<E>(), E::TOPIC));
        self.slots.push(TopicSlot {
            // The receiver ends are handed out by `subscribe`; the bus
            // keeps the sole sender so the topic lives as long as the bus.
            tx: Box::new(broadcast::channel::<Sequenced<E>>(capacity).0),
            name: E::TOPIC,
            metrics: Arc::new(TopicMetrics::default()),
            send_order: Mutex::new(()),
        });
        self
    }

    /// Freeze the registry: build the TypeId index once; every later
    /// publish pays one hash lookup against it.
    pub(crate) fn build(self) -> Result<EventBus, RegistryError> {
        let mut index = HashMap::with_capacity(self.slots.len());
        for (i, (key, name)) in self.keys.into_iter().enumerate() {
            if index.insert(key, i).is_some() {
                return Err(RegistryError::Duplicate { type_name: name });
            }
        }
        Ok(EventBus {
            slots: self.slots,
            index,
        })
    }
}

/// A topic subscription. Wraps the raw broadcast receiver so every
/// `Lagged` lands in the topic's metrics (the loss-detection leg lives
/// once in the core). Stream-based consumers take
/// [`TopicReceiver::into_stream`], which keeps that counting leg.
/// The receiver also tracks its last-delivered seq so a `Lagged` can
/// report the lost range, not just the count.
pub(crate) struct TopicReceiver<E> {
    rx: broadcast::Receiver<Sequenced<E>>,
    metrics: Arc<TopicMetrics>,
    name: &'static str,
    /// Seq of the last event handed out; anchors lag-range math.
    last_delivered: Option<u64>,
}

impl<E: Event> TopicReceiver<E> {
    /// Receive one event. A lagged receiver still gets its `Lagged(n)`
    /// error (callers keep their existing recovery arms) and the topic's
    /// lagged counter absorbs the loss.
    pub(crate) async fn recv(&mut self) -> Result<E, RecvError> {
        match self.rx.recv().await {
            Ok(sequenced) => {
                self.last_delivered = Some(sequenced.seq);
                Ok(sequenced.event)
            }
            Err(RecvError::Lagged(n)) => {
                record_loss(&self.metrics, &mut self.last_delivered, self.name, n);
                Err(RecvError::Lagged(n))
            }
            Err(err) => Err(err),
        }
    }

    /// Non-blocking twin of [`TopicReceiver::recv`] with the same counting.
    pub(crate) fn try_recv(&mut self) -> Result<E, TryRecvError> {
        match self.rx.try_recv() {
            Ok(sequenced) => {
                self.last_delivered = Some(sequenced.seq);
                Ok(sequenced.event)
            }
            Err(TryRecvError::Lagged(n)) => {
                record_loss(&self.metrics, &mut self.last_delivered, self.name, n);
                Err(TryRecvError::Lagged(n))
            }
            Err(err) => Err(err),
        }
    }

    /// Stream form of the subscription: wraps the raw receiver the way
    /// `into_inner` did, but keeps the `Lagged` counting leg — the topic's
    /// counter stays the single loss-detection site for stream-based
    /// consumers (the SSE assembler's merge). Losses surface as skips,
    /// the same semantics the merge applies to lagged sources.
    pub(crate) fn into_stream(self) -> impl Stream<Item = E> + Send {
        let metrics = Arc::clone(&self.metrics);
        let name = self.name;
        let mut last_delivered = self.last_delivered;
        BroadcastStream::new(self.rx).filter_map(move |result| match result {
            Ok(sequenced) => {
                last_delivered = Some(sequenced.seq);
                Some(sequenced.event)
            }
            Err(BroadcastStreamRecvError::Lagged(n)) => {
                record_loss(&metrics, &mut last_delivered, name, n);
                None
            }
        })
    }
}

/// Shared lag leg for all three consumption paths (`recv`, `try_recv`,
/// `into_stream`): the loss lands in the topic's `lagged` counter (the
/// health cell, unchanged) and the lost seq range is recorded when both
/// bounds are known — i.e. when this receiver has already delivered at
/// least one event (`last_delivered` anchors the interval).
/// Lagging before any delivery is the honest degraded case:
/// counted and warned, range unknown. One warn per gap.
fn record_loss(
    metrics: &TopicMetrics,
    last_delivered: &mut Option<u64>,
    name: &'static str,
    n: u64,
) {
    metrics.lagged.fetch_add(n, Ordering::Relaxed);
    match *last_delivered {
        Some(last) => {
            let range = (last + 1, last + n);
            *last_delivered = Some(last + n);
            *metrics.last_gap.lock().unwrap_or_else(|e| e.into_inner()) = Some(range);
            tracing::warn!(
                topic = name,
                lost = ?range,
                delta = n,
                "topic lag: events dropped by a slow receiver"
            );
        }
        None => {
            tracing::warn!(
                topic = name,
                delta = n,
                "topic lag: events dropped by a slow receiver, range unknown (no prior delivery)"
            );
        }
    }
}
#[cfg(test)]
mod tests;
