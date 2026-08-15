//! The single external projector: the SOLE writer of chat content.
//!
//! It owns the unified `chat_history` store + the resolved conversation id and
//! is the one place a durable row is written. Every ingress that produces chat
//! content funnels through it, and each write publishes the stamped frame onto
//! one broadcast (the "external bus") that the serving paths — the direct local
//! SSE and the relay E2EE envelope — subscribe to and forward. The pumps never
//! touch `chat_history`; they are pure forwarders. This makes "the write
//! produces the event" the uniform rule for both directions and dissolves the
//! double-write that coupling persistence to the pumps would otherwise cause
//! once the two paths share one store.
//!
//! Ingress:
//! - **Agent runtime replies** — the projector subscribes to the root agent's
//!   raw `SseEvent` broadcast (the single subscription that the two pumps used
//!   to duplicate), projects each event via [`crate::projector::project_external`],
//!   persists the authored half once, and publishes it. The signal half is
//!   published without persistence (ephemeral).
//! - **Agent `send` CLI** — [`ExternalProjector::record_outbound`] (called by
//!   the lesche message route), which burst-limits, persists, and publishes.
//! - **Inbound user message** — [`ExternalProjector::record_inbound`] (called
//!   by the shared `deliver_message` seam before the prompt enqueues), which
//!   appends the row and publishes a stamped `UserMessage` frame so the
//!   frontend can promote its optimistic line.
//!
//! Status snapshots are deliberately NOT routed here: they are ephemeral, on a
//! fixed cadence, owned by the direct/relay status pumps.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use kallip_agora_common::ids::{ConversationId, ParticipantId, ParticipantKind, TagmaId};
use kallip_common::protocol::SseEvent;
use kallip_lesche_common::message::{HistoryEntry, Participant, TagmaReply};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::relay::MessageLimits;
use crate::relay::RelayMessageError;
use crate::relay::chat_history::{self, Db};
use crate::relay::ops::MessageLimiter;
use crate::state::{AppState, RegistryEntry};

/// Broadcast capacity for the external bus. Mirrors the prior direct pump's
/// capacity. A `Lagged` receiver now loses a frame for BOTH transports at once
/// (whereas the old two independent pumps lagged independently); accepted,
/// because either transport recovers on its next `history({after:
/// maxRendered})` pull.
const FRAMES_CAPACITY: usize = 256;

/// One frame on the external bus the serving paths subscribe to. The sender
/// rides alongside the content (never inside it): the projector stamps it once
/// per direction (agent outbound, user inbound echo), and each serving path
/// copies it onto its carrier — the relay envelope's `sender` (online) or the
/// `DirectFrame::Authored.sender` (offline).
#[derive(Clone, Debug)]
pub(crate) enum ExternalFrame {
    /// Authored content, persisted once and stamped with its `history_id`,
    /// paired with the sender who authored it.
    Authored {
        sender: Participant,
        reply: TagmaReply,
    },
    /// A runtime signal (busy/idle/terminals/errors). Ephemeral, never persisted,
    /// and carries no sender (operator metadata, not conversation content).
    Signal(kallip_common::protocol::SignalEvent),
}

/// The single external projector handle. Cheap to clone (`Arc` inside); the
/// pumps and routes hold clones to subscribe / record.
#[derive(Clone)]
pub(crate) struct ExternalProjector {
    inner: Arc<Inner>,
}

struct Inner {
    frames_tx: broadcast::Sender<ExternalFrame>,
    history: Option<Db>,
    /// The tagma's own identity — used to stamp the agent sender on outbound
    /// frames and to refine a backfilled outbound row's empty `sender_id` on
    /// replay. `tagma_id` is write-once (known at boot when creds existed, else
    /// resolved at first-run enrollment); `tagma_label` is the enrolled label
    /// (or the `"Tagma"` fallback for a never-enrolled offline-only tagma).
    tagma_id: OnceLock<TagmaId>,
    tagma_label: OnceLock<String>,
    /// The conversation id surfaced on the root-agent summary as the frontend
    /// cache key (so the direct and relay paths share one IndexedDB cache).
    /// Write-once: set at construction when the tagma id is known at boot
    /// (creds existed), else filled by
    /// [`ExternalProjector::set_conversation_id`] the moment first-run
    /// enrollment resolves the id. `unset` for a never-enrolled tagma (no
    /// cache-key id), though rows are still written to the `NULL` operator
    /// partition. `OnceLock` (not `Mutex`): write-once-read-many on the hot
    /// path, no lock and no await hazard.
    conversation_id: OnceLock<ConversationId>,
    /// Burst cap on the agent's outbound `send` (mirrors the prior per-pump
    /// limiter). Inbound user messages are not capped here.
    message_limiter: Mutex<MessageLimiter>,
    /// The current turn's peer partition for outbound rows: `None` = the
    /// operator (direct path), `Some(peer)` = a relay user. Set on each
    /// `record_inbound` and never cleared (clearing on `Idle` races the next
    /// turn's outbound). Single-conversation runtime only; concurrent
    /// multi-conversation turns will carry the partition per turn. Two latent
    /// consequences of "last inbound wins" until then: interleaved direct +
    /// relay turns can stamp an outbound under the wrong peer, and a proactive
    /// outbound with no preceding inbound (e.g. a cron prompt right after
    /// restart) lands in the `NULL` operator partition.
    partition: Mutex<Option<Participant>>,
    state: Weak<AppState>,
}

/// The fallback agent handle when no enrolled label is known (a never-enrolled
/// offline-only tagma). Matches the migration's backfill default.
const FALLBACK_AGENT_HANDLE: &str = "Tagma";

/// The reserved tagma id used to derive the agent `ParticipantId` when the
/// enrolled id is unresolved (a never-enrolled tagma). Stable across restarts
/// and distinct from any agora-assigned id; `agent_sender().tagma_id` stays
/// `None` to signal "not enrolled" honestly.
fn offline_tagma_id() -> TagmaId {
    "local-tagma".parse().unwrap()
}

/// The operator's wire sender on the direct path. `NULL` in storage denotes
/// the operator (no identity); the wire always carries a resolved
/// `Participant`, so the operator is represented by this nil-id, empty-handle
/// participant. The id is a non-rendered placeholder (the frontend suppresses
/// the sender label on own messages), and the empty handle reflects the
/// anonymous operator — no identity to carry.
fn operator_sender() -> Participant {
    Participant {
        id: ParticipantId::from(uuid::Uuid::nil().to_string()),
        kind: ParticipantKind::Human,
        handle: String::new(),
        tagma_id: None,
    }
}

/// Decompose a peer partition into the stored `(user_id, username)` pair.
/// `None` (the operator) maps to `(None, None)` (the `NULL` partition).
fn peer_fields(partition: &Option<Participant>) -> (Option<String>, Option<String>) {
    match partition {
        Some(p) => (Some(p.id.as_ref().to_string()), Some(p.handle.clone())),
        None => (None, None),
    }
}

/// Resolve the wire sender for a stored row: outbound ⇒ the agent; inbound ⇒
/// the peer (`user_id`/`username`), or the operator for the `NULL` partition.
fn resolve_sender(row: &chat_history::HistoryRow, agent: &Participant) -> Participant {
    if row.direction == "outbound" {
        return agent.clone();
    }
    match &row.user_id {
        Some(id) => Participant {
            id: ParticipantId::from(id.clone()),
            kind: ParticipantKind::Human,
            handle: row.username.clone().unwrap_or_default(),
            tagma_id: None,
        },
        None => operator_sender(),
    }
}

impl ExternalProjector {
    /// Construct the handle and spawn the event pump that subscribes to the
    /// root agent's broadcast. `history` is always `Some` in production (the Db
    /// is opened unconditionally at boot). `conversation_id` / `tagma_id` are
    /// `Some` when known at boot (creds existed); `None` on the first-run
    /// enroll boot, filled later by their setters. The pump is cancelled by a
    /// child of `state.shutdown`.
    pub(crate) fn new(
        state: Weak<AppState>,
        history: Option<Db>,
        conversation_id: Option<ConversationId>,
        tagma_id: Option<TagmaId>,
        tagma_label: Option<String>,
        message_limits: MessageLimits,
    ) -> Self {
        let (frames_tx, _) = broadcast::channel(FRAMES_CAPACITY);
        let cancel = state
            .upgrade()
            .map(|s| s.shutdown.child_token())
            .unwrap_or_default();
        let conversation_id_once = match conversation_id {
            Some(c) => OnceLock::from(c),
            None => OnceLock::new(),
        };
        let tagma_id_once = match tagma_id {
            Some(t) => OnceLock::from(t),
            None => OnceLock::new(),
        };
        let tagma_label_once = match tagma_label {
            Some(l) => OnceLock::from(l),
            None => OnceLock::new(),
        };
        let inner = Arc::new(Inner {
            frames_tx,
            history,
            tagma_id: tagma_id_once,
            tagma_label: tagma_label_once,
            conversation_id: conversation_id_once,
            message_limiter: Mutex::new(MessageLimiter::new(message_limits)),
            partition: Mutex::new(None),
            state,
        });
        let projector = Self {
            inner: inner.clone(),
        };
        tokio::spawn(projector.clone().run_event_pump(cancel));
        projector
    }

    /// Set the tagma id once first-run enrollment resolves it. Write-once: no-op
    /// if already set.
    pub(crate) fn set_tagma_id(&self, id: TagmaId) {
        let _ = self.inner.tagma_id.set(id);
    }

    /// The agent sender for outbound frames: the tagma id + its label. The agent
    /// exists (it is this tagma), so its id/handle are always real; only the
    /// enrolled `tagma_id` is honestly `None` for a never-enrolled tagma (the
    /// signal that the agora-assigned id is unresolved), using a reserved
    /// offline id for the derivation in the meantime.
    fn agent_sender(&self) -> Participant {
        let resolved = self.inner.tagma_id.get().cloned();
        let id = resolved.clone().unwrap_or_else(offline_tagma_id);
        let handle = self
            .inner
            .tagma_label
            .get()
            .cloned()
            .unwrap_or_else(|| FALLBACK_AGENT_HANDLE.to_string());
        Participant {
            id: ParticipantId::for_tagma(&id),
            kind: ParticipantKind::Agent,
            handle,
            tagma_id: resolved,
        }
    }

    /// Set the conversation id once first-run enrollment resolves the tagma id.
    /// Write-once: no-op if already set (the loaded-creds branch constructed
    /// with the id; only the enroll branch calls this, once).
    pub(crate) fn set_conversation_id(&self, conv: ConversationId) {
        let _ = self.inner.conversation_id.set(conv);
    }

    /// Subscribe to the external bus. Each serving path (direct SSE, relay
    /// envelope) subscribes once and forwards the frames it receives.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ExternalFrame> {
        self.inner.frames_tx.subscribe()
    }

    /// The resolved conversation id for this tagma, if any (`None` until
    /// enrollment resolves it). Surfaced on the root-agent summary so the
    /// offline frontend can key its cache + history pulls under the same id the
    /// relay uses.
    pub(crate) fn conversation_id(&self) -> Option<String> {
        self.inner
            .conversation_id
            .get()
            .map(|c| c.as_ref().to_string())
    }

    /// Publish a frame; a `Send` error means there are currently no subscribers,
    /// which is benign (the frame is live-only and needs no durable echo — the
    /// persisted rows are re-pullable via history).
    fn publish(&self, frame: ExternalFrame) {
        let _ = self.inner.frames_tx.send(frame);
    }

    /// Run the outbound burst check WITHOUT persisting or publishing. The
    /// room send bypasses the bilateral projector (no `chat_history` row, no
    /// bilateral frame) but shares this one rate limit, because both paths are
    /// the same agent voice: one per-tagma burst cap covers bilateral + room
    /// output together, rather than letting the two paths double the cap. The
    /// room path calls this, then does its own `post_room_envelope`.
    pub(crate) async fn check_outbound_burst(&self) -> bool {
        self.inner.message_limiter.lock().await.check()
    }

    /// The agent speaking to the user (`kallip lesche send`). Burst-limited,
    /// persisted once as outbound, and published as a stamped `AssistantContent`
    /// frame carrying the agent sender. Returns [`RelayMessageError::BurstExceeded`]
    /// when the cap is hit; no `Delivery` variant is produced (the projector does
    /// not POST).
    pub(crate) async fn record_outbound(&self, text: String) -> Result<(), RelayMessageError> {
        let allowed = self.inner.message_limiter.lock().await.check();
        if !allowed {
            return Err(RelayMessageError::BurstExceeded);
        }
        let sender = self.agent_sender();
        let mut reply = TagmaReply::Event {
            event: kallip_common::protocol::AuthoredEvent::AssistantContent {
                content: text.clone(),
            },
            history_id: 0,
            created_at: None,
        };
        self.stamp(&text, &mut reply).await;
        self.publish(ExternalFrame::Authored { sender, reply });
        Ok(())
    }

    /// A user message entering the conversation. `partition` is the peer:
    /// `None` = the operator (direct path, stored as `NULL`), `Some(peer)` = a
    /// relay user (stored as `user_id`/`username`). Appended once as an inbound
    /// row, then published as a stamped `UserMessage` frame so the frontend
    /// promotes its optimistic line. Called by the shared `deliver_message`
    /// seam BEFORE the prompt enqueues, so the row is durable even if the agent
    /// reply races. Also records the turn's partition so the agent's outbound
    /// reply lands in the same conversation.
    ///
    /// No persist gate: a row is always written when the store is present (the
    /// peer partition is the key, and it is always known at ingest — `None` for
    /// the operator). The only unstamped path is a genuine append failure.
    pub(crate) async fn record_inbound(&self, partition: Option<Participant>, text: String) {
        *self.inner.partition.lock().await = partition.clone();
        let sender = partition.clone().unwrap_or_else(operator_sender);
        let Some(db) = self.inner.history.clone() else {
            self.publish_unstamped_inbound(sender, text);
            return;
        };
        let (user_id, username) = peer_fields(&partition);
        match chat_history::append(
            &db,
            user_id.as_deref(),
            username.as_deref(),
            "inbound",
            &text,
        )
        .await
        {
            Ok((id, created_at)) => {
                let mut reply = TagmaReply::UserMessage {
                    history_id: id,
                    text,
                    created_at: None,
                };
                reply.set_history_id(id);
                reply.set_created_at(created_at);
                self.publish(ExternalFrame::Authored { sender, reply });
            }
            Err(e) => {
                error!("inbound history append failed: {e:#}");
                self.publish_unstamped_inbound(sender, text);
            }
        }
    }

    /// Echo the inbound text unstamped when persistence is unavailable, so live
    /// delivery is not lost (mirrors the outbound graceful-degrade rule).
    fn publish_unstamped_inbound(&self, sender: Participant, text: String) {
        self.publish(ExternalFrame::Authored {
            sender,
            reply: TagmaReply::UserMessage {
                history_id: 0,
                text,
                created_at: None,
            },
        });
    }

    /// Read a history window for one peer partition as decoded entries (each a
    /// sender and reply) with a `more` flag. Used by the direct
    /// `/external/history` endpoint and the relay history replay (the single
    /// read path). `user_id = None` reads the operator (direct) partition;
    /// `Some(id)` reads that peer's relay partition. `more` is true only for
    /// paginated (`after`/`before`) queries that returned a full page; the
    /// recent-N snapshot is always `more=false`. The wire sender is
    /// reconstructed per row: outbound becomes the agent, inbound becomes the
    /// peer (or the operator for `NULL`).
    pub(crate) async fn read_history(
        &self,
        user_id: Option<&str>,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) -> (Vec<HistoryEntry>, bool) {
        let Some(db) = self.inner.history.clone() else {
            return (Vec::new(), false);
        };
        let rows = match (after, before) {
            (Some(a), None) => chat_history::read_after(&db, user_id, a, limit).await,
            (None, Some(b)) => chat_history::read_before(&db, user_id, b, limit).await,
            (None, None) => chat_history::read_last_n(&db, user_id, limit as u64).await,
            (Some(_), Some(_)) => Ok(Vec::new()),
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                error!("history read failed: {e:#}");
                return (Vec::new(), false);
            }
        };
        let more = match (after, before) {
            (None, None) => false,
            _ => rows.len() as u32 == limit && limit > 0,
        };
        let agent = self.agent_sender();
        let entries = rows
            .into_iter()
            .filter_map(|r| {
                let sender = resolve_sender(&r, &agent);
                chat_history::decode_row(r, sender)
            })
            .collect();
        (entries, more)
    }

    /// Append an outbound row once and stamp its row id + `created_at` onto
    /// `reply`. The row keys on the current turn's partition (the agent speaks
    /// *to* the peer); the agent's own identity is not stored (reconstructed at
    /// read). Graceful-degrade: a failure leaves `history_id` at 0 and the
    /// caller still publishes the frame live.
    async fn stamp(&self, text: &str, reply: &mut TagmaReply) {
        let Some(db) = self.inner.history.clone() else {
            return;
        };
        let partition = self.inner.partition.lock().await.clone();
        let (user_id, username) = peer_fields(&partition);
        if let Ok((id, created_at)) = chat_history::append(
            &db,
            user_id.as_deref(),
            username.as_deref(),
            "outbound",
            text,
        )
        .await
        {
            reply.set_history_id(id);
            reply.set_created_at(created_at);
        }
    }

    /// The event pump: subscribe to the root agent's raw broadcast, project each
    /// event, persist the authored half once, and publish authored + signal
    /// frames. Retries until the root is live (it may be transiently absent
    /// during restore/reactivation). Mirrors the prior two pumps' retry loops.
    async fn run_event_pump(self, cancel: CancellationToken) {
        let mut rx = loop {
            if let Some(rx) = self.subscribe_root().await {
                break rx;
            }
            if cancel.is_cancelled() {
                return;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        };
        // NOTE: this start/stop pair is one-per-tagma-lifetime (boot-bounded),
        // unlike `relay/pump.rs` which cycles on every re-KEX — so INFO is
        // defensible here. Do not "fix" it the same way the relay pump was
        // demoted to DEBUG.
        info!("external projector event pump started");
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("external projector event pump stopped");
                    return;
                }
                recv = rx.recv() => match recv {
                    Ok(sse) => {
                        let (authored, signal) = crate::projector::project_external(&sse);
                        if let Some(event) = authored {
                            let sender = self.agent_sender();
                            // The assistant content is the only authored
                            // variant today; extract it so the typed store can
                            // persist `text` as a column. A future structured
                            // variant extends `stamp` / the schema.
                            let text = match &event {
                                kallip_common::protocol::AuthoredEvent::AssistantContent {
                                    content,
                                } => content.clone(),
                            };
                            let mut reply = TagmaReply::Event {
                                event,
                                history_id: 0,
                                created_at: None,
                            };
                            self.stamp(&text, &mut reply).await;
                            self.publish(ExternalFrame::Authored { sender, reply });
                        }
                        if let Some(signal) = signal {
                            self.publish(ExternalFrame::Signal(signal));
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(lagged = n, "external projector lagged events");
                    }
                    Err(RecvError::Closed) => {
                        warn!("external projector: root stream closed; re-subscribing");
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                        rx = match self.subscribe_root().await {
                            Some(r) => r,
                            None => return,
                        };
                    }
                }
            }
        }
    }

    /// Resolve the root agent's `events_tx` under the registry read-lock and
    /// subscribe. Returns `None` if the tagma is shutting down or the root is
    /// not live.
    async fn subscribe_root(&self) -> Option<broadcast::Receiver<SseEvent>> {
        let state = self.inner.state.upgrade()?;
        let registry = state.registry.read().await;
        let (_id, entry) = registry.root_agent()?;
        match entry {
            RegistryEntry::Live(live) => Some(live.agent.events_tx.subscribe()),
            RegistryEntry::Faulted(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
