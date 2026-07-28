//! The relay connector: the optional online-mode subsystem that links the
//! tagma to agora/lesche. Folded in from the former standalone connector — the
//! tagma now hosts it in-process.
//!
//! Responsibilities (ported from the former standalone connector):
//! - hold the lesche tunnel SSE + reconnect loop (`run` / `connect_and_drain`);
//! - broker the per-conversation E2E key (KEX) and the AEAD epoch (`handle_kex`,
//!   `crypto::CryptoState`);
//! - decrypt inbound app ops and run them against the root agent in-process
//!   (`handle_user_op` / `execute_op`);
//! - forward the external projector's authored/signal bus onto the relay:
//!   encrypted envelope for authored content, plaintext for signals (`pump`).
//!
//! The projector (see [`crate::external`]) is the sole writer of chat content;
//! this module only encrypts + posts. The E2E key never leaves this process.
//! `Inner` holds a `Weak<AppState>` (not a strong ref) to avoid a reference
//! cycle with `AppState.relay`.

pub(crate) mod chat_history;
mod crypto;
mod kex;
pub(crate) mod ops;
mod pump;
pub(crate) mod status_pump;

use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::ids::{ConversationId, TagmaId};
use kallip_e2ee::{self as e2e, DeviceKey};
use kallip_lesche_client::LescheClient;
use kallip_lesche_common::message::{
    Envelope, Participant, TagmaControl, TagmaReply, TagmaRequest,
};
use kallip_lesche_common::tunnel::TunnelInbound;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use kallip_common::agentid::AgentId;

use crate::auth::Identity;
use crate::state::{AppState, SharedState};

use crypto::CryptoState;
use ops::{op_err_reply, op_trace};

// Re-exported so `activate_relay` (main.rs) can construct the configured limits.
pub(crate) use ops::{DEFAULT_MESSAGE_BURST_MAX, DEFAULT_MESSAGE_BURST_WINDOW, MessageLimits};

/// Error from a message-delivery attempt.
#[derive(Debug, thiserror::Error)]
pub enum RelayMessageError {
    /// The process-global message burst cap was exceeded.
    #[error("message burst cap exceeded")]
    BurstExceeded,
    /// Encrypting / posting the envelope failed.
    #[error(transparent)]
    Delivery(#[from] anyhow::Error),
}

/// The running pump task plus the token that stops it.
struct PumpHandle {
    task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct RelayHandle {
    inner: Arc<Inner>,
}

struct Inner {
    tagma_id: TagmaId,
    conversation_id: ConversationId,
    /// The data-plane relay client (tunnel SSE, envelope + KEX POSTs). Owns the
    /// two reqwest clients internally (POST 30s + stream no-total-timeout).
    client: LescheClient,
    device: DeviceKey,
    root_agent: AgentId,
    /// AEAD session key + both sequence counters, under one lock.
    crypto: Mutex<CryptoState>,
    /// The running event pump, if any. Restarted on each KEX so a re-KEX can
    /// reset the outbound counter with no in-flight emits under the old key.
    pump: Mutex<Option<PumpHandle>>,
    /// The running status pump, if any. Bounded to the tunnel's lifetime
    /// (started on tunnel-up, stopped on tunnel-down), NOT the KEX epoch --
    /// status is plaintext and key-independent.
    status_pump: Mutex<Option<PumpHandle>>,
    /// In-flight per-envelope op tasks, so shutdown can abort and drain them
    /// rather than leaving them fire-and-forget. See [`RelayHandle::stop_dispatch`].
    dispatch: Mutex<tokio::task::JoinSet<()>>,
    /// `Weak` to break the `RelayHandle` ↔ `AppState` reference cycle. Upgraded
    /// at call time; `None` during shutdown → the op fails gracefully.
    state: Weak<AppState>,
}

impl RelayHandle {
    pub fn new(
        client: LescheClient,
        tagma_id: TagmaId,
        device: DeviceKey,
        root_agent: AgentId,
        state: Weak<AppState>,
    ) -> Self {
        let conversation_id = ConversationId::for_tagma(&tagma_id);
        Self {
            inner: Arc::new(Inner {
                tagma_id,
                conversation_id,
                client,
                device,
                root_agent,
                crypto: Mutex::new(CryptoState::new()),
                pump: Mutex::new(None),
                status_pump: Mutex::new(None),
                dispatch: Mutex::new(tokio::task::JoinSet::new()),
                state,
            }),
        }
    }

    /// The tagma id this relay enrolled as (for diagnostics / compose wiring).
    pub fn tagma_id(&self) -> &TagmaId {
        &self.inner.tagma_id
    }

    /// Hold the lesche tunnel open, reconnecting with a small backoff on any
    /// disconnect or error. Selects on `shutdown` (the tagma-wide parent token)
    /// so SIGINT/SIGTERM cancels the relay alongside axum and the agents. On
    /// shutdown the pump is drained (`stop_pump`) so in-flight emits complete.
    ///
    /// Cancel-safety: `connect_and_drain` and the per-op `tokio::spawn(dispatch)`
    /// are cancel-safe at every `.await` — a cancel mid-op loses the partial op,
    /// which the app retries via host-history re-pull on reconnect.
    pub async fn run(self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.stop_workers().await;
                    return;
                }
                r = self.clone().connect_and_drain() => match r {
                    Ok(()) => info!("relay tunnel stream ended; reconnecting"),
                    Err(e) => warn!("relay tunnel error: {e:#}; reconnecting"),
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.stop_workers().await;
                    return;
                }
                _ = tokio::time::sleep(ops::TUNNEL_RECONNECT_BACKOFF) => {}
            }
        }
    }

    /// Drain both the pump and the in-flight op-dispatch tasks. Called from the
    /// shutdown branches of [`RelayHandle::run`].
    async fn stop_workers(&self) {
        self.stop_pump().await;
        self.stop_status_pump().await;
        self.stop_dispatch().await;
    }

    /// Abort and reap all in-flight op-dispatch tasks. Only safe on a process-
    /// tearing-down path: `deliver_message`'s spawn step (spawn-agent → install)
    /// is not abort-safe mid-flight, so aborting a dispatch there can leak
    /// spawned tasks or leave a disarmed workspace lock. Both `run` shutdown
    /// branches are process-exit paths, so this is acceptable; a future
    /// non-shutdown caller must first make `deliver_message` abort-safe.
    async fn stop_dispatch(&self) {
        let mut set = self.inner.dispatch.lock().await;
        set.abort_all();
        while set.join_next().await.is_some() {}
    }

    /// Open the tunnel SSE and dispatch each inbound message (each on its own
    /// task so a long-running op does not stall the stream reader). The status
    /// pump is bounded to this tunnel session: started once the tunnel is up,
    /// stopped when the stream ends so a reconnect installs a fresh pump.
    async fn connect_and_drain(self) -> Result<()> {
        let stream = self
            .inner
            .client
            .open_tunnel(&self.inner.device, &self.inner.tagma_id)
            .await?;
        self.start_status_pump().await;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            match item {
                Ok(inbound) => {
                    self.inner
                        .dispatch
                        .lock()
                        .await
                        .spawn(self.clone().dispatch(inbound));
                }
                Err(e) => warn!("relay tunnel stream error: {e}"),
            }
        }
        self.stop_status_pump().await;
        Ok(())
    }

    async fn dispatch(self, inbound: TunnelInbound) {
        match inbound {
            TunnelInbound::KeyExchange {
                conversation_id,
                init,
            } => self.handle_kex(conversation_id, init).await,
            TunnelInbound::Envelope { envelope } => {
                // Outer last-resort: a panic that escapes the inner req_id-aware
                // boundaries in `handle_agent_op` / `handle_history`. Those cover
                // the common case (a panic yields a correlated reply/marker); a
                // panic reaching here means req_id was never parsed (or an
                // invariant broke after it), so we can only log.
                if AssertUnwindSafe(self.handle_user_op(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    warn!("relay op dispatch panicked past the inner boundary");
                }
            }
        }
    }

    /// Respond to a key exchange. This is also the re-KEX boundary: cancel any
    /// running pump, install the new key with reset counters, then (re)start the
    /// pump. Cancelling the pump first guarantees no emit using the old key is
    /// in flight when the counter resets, so re-KEX cannot reuse a nonce.
    async fn handle_kex(
        &self,
        conversation_id: ConversationId,
        init: kallip_lesche_common::control::KeyExchangeInit,
    ) {
        let (response, key) = match kex::respond_key_exchange(
            &self.inner.device,
            self.inner.tagma_id.as_ref(),
            conversation_id.as_ref(),
            &init,
        ) {
            Ok(x) => x,
            Err(e) => {
                warn!(conv = %conversation_id, "key exchange failed: {e:#}");
                return;
            }
        };
        // Quiesce the pump before mutating crypto state: its in-flight emits
        // (if any) used the old key and must drain before we rotate + reset.
        self.stop_pump().await;
        // `first` distinguishes the initial KEX from a re-KEX (the cadence the
        // user sees in the logs); a re-KEX means a prior key was already installed.
        let first = {
            let mut c = self.inner.crypto.lock().await;
            let had_key = c.key.is_some();
            c.key = Some(key);
            c.outbound_seq = 0;
            c.seen_inbound = None;
            !had_key
        };
        self.start_pump().await;
        if let Err(e) = self
            .inner
            .client
            .post_key_exchange_response(&conversation_id, &response)
            .await
        {
            warn!(conv = %conversation_id, "post key-exchange response: {e:#}");
        }
        // `conv` is the relay-plane key (what agora/lesche index this KEX by),
        // so it carries the cross-service correlation value; the tagma id is a
        // constant 1:1 derivation of it and adds nothing per-line.
        info!(conv = %conversation_id, first, "relay KEX completed");
        // History is pull-based: the app sends a `TagmaControl::History` request
        // once it has hydrated its local cache, so KEX no longer auto-replays.
    }

    /// Decrypt an app-originated envelope and dispatch it: a `TagmaRequest`
    /// drives the root agent (and emits its reply); a `TagmaControl` is a
    /// sync/plumbing op (today: history pull). The agent-driving branch runs
    /// under a req_id-aware panic boundary so a bug never leaves the app
    /// hanging: a panic yields an `Error` reply for the exact `req_id`.
    ///
    /// `req_id` is peeled from the decoded JSON *before* the branch so the
    /// outer `catch_unwind` (in [`Self::dispatch`]) can attribute a panic that
    /// occurs anywhere downstream — both `TagmaRequest` and `TagmaControl`
    /// carry a top-level `req_id`.
    async fn handle_user_op(&self, envelope: Envelope) {
        let Participant::User { .. } = &envelope.sender else {
            return; // only user→tagma envelopes drive ops
        };
        let plain = {
            let mut c = self.inner.crypto.lock().await;
            // Replay check: drop a seq at or below the epoch high-water-mark.
            // The mark is advanced only AFTER a successful decrypt below, so a
            // forged envelope with a large `sequence_n` and undecryptable
            // ciphertext cannot poison the window for the rest of the epoch.
            if let Some(prev) = c.seen_inbound
                && envelope.sequence_n <= prev
            {
                warn!(
                    seq = envelope.sequence_n,
                    "replayed inbound envelope; dropping"
                );
                return;
            }
            let key = match &c.key {
                Some(k) => k,
                None => {
                    warn!("op envelope before key exchange; dropping");
                    return;
                }
            };
            // Decrypt under the lock so the window advance and the decrypt share
            // one atomic decision. `decrypt` is pure CPU (no await), so holding
            // the crypto Mutex across it does not stall the runtime.
            let plain = match e2e::decrypt(key, envelope.sequence_n, &envelope.ciphertext.0) {
                Some(p) => p,
                None => {
                    warn!("op decrypt failed; dropping");
                    return;
                }
            };
            // Authenticated: advance the epoch high-water-mark.
            c.seen_inbound = Some(envelope.sequence_n);
            plain
        };
        // Parse once to a Value, then dispatch by the `op` discriminant. The
        // `op` tags of `TagmaRequest` (send_message/interrupt) and
        // `TagmaControl` (history) are disjoint (pinned by
        // `request_and_control_op_tags_are_disjoint`), so this routing is total.
        let value: serde_json::Value = match serde_json::from_slice(&plain) {
            Ok(v) => v,
            Err(e) => {
                warn!("op decode failed: {e}");
                return;
            }
        };
        let req_id = value.get("req_id").and_then(|v| v.as_u64()).unwrap_or(0);
        let trace = op_trace(req_id);
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "send_message" | "interrupt" => {
                let request: TagmaRequest = match serde_json::from_value(value) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(req_id, "request decode failed: {e}");
                        return;
                    }
                };
                self.handle_agent_op(&trace, req_id, request).await;
            }
            "history" => {
                let ctrl: TagmaControl = match serde_json::from_value(value) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(req_id, "control decode failed: {e}");
                        return;
                    }
                };
                let TagmaControl::History {
                    after,
                    before,
                    limit,
                    ..
                } = ctrl;
                self.handle_history(&trace, req_id, after, before, limit)
                    .await;
            }
            other => warn!(req_id, op = other, "unknown op; dropping"),
        }
    }

    /// Run a `TagmaRequest` against the root agent and emit the reply, under a
    /// req_id-aware panic boundary. The inbound `SendMessage` is persisted by
    /// the external projector inside `deliver_message` (the shared seam
    /// `execute_op` calls), which also publishes the stamped `UserMessage` frame
    /// the pump forwards — so the app dedups its optimistic user line off that
    /// frame, not this ack. The `MessageAccepted` reply therefore carries
    /// `history_id = 0`. `Interrupt` carries no content and is not stored — its
    /// visible effect is the agent's outbound `Interrupted` event, which the
    /// pump already forwards.
    async fn handle_agent_op(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        request: TagmaRequest,
    ) {
        let result = AssertUnwindSafe(self.execute_op(request))
            .catch_unwind()
            .await;
        let reply = match result {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => {
                warn!(req_id, "op failed: {e:#}");
                op_err_reply(req_id, &e)
            }
            Err(_) => {
                warn!(req_id, "op panicked; emitting 502");
                TagmaReply::Error {
                    req_id,
                    status: 502,
                    message: "relay op panicked".to_string(),
                }
            }
        };
        // Op replies pass `cancel: None`: an in-flight reply POST is left to
        // complete across a re-KEX rather than aborted. This is safe by key
        // rotation (a re-KEX swaps the AEAD key, so an old-key envelope the app
        // receives post-rotation is simply undecryptable and dropped), and
        // aborting would discard a user-visible ack. The pump path, by contrast,
        // passes its cancel token so a slow event emit cannot stall a re-KEX.
        if let Err(e) = self.emit(trace, reply, None).await {
            warn!(req_id, "emit reply: {e:#}");
        }
    }

    /// Hard cap on the number of rows one `History` batch returns, regardless
    /// of the `limit` the app requests. Bounds the per-batch POST time (each
    /// row is a separate encrypted POST) so a misbehaving client cannot stall
    /// delivery. The app paginates with `after`/`before` for more.
    const HISTORY_BATCH_MAX: u32 = 50;

    /// Respond to a `TagmaControl::History` request: read the matching window
    /// from `chat_history` (no crypto lock), re-encrypt each row under the
    /// current epoch key, POST it, then POST a `HistoryBatchEnd` marker. The
    /// marker is the *sole* completion signal — if any POST in the batch fails
    /// (503 exhausted), the loop stops WITHOUT the marker, and the app times
    /// out and retries on the next reconnect (its cursor has not advanced past
    /// the un-delivered rows). Lock discipline mirrors [`Self::emit`]: SQLite
    /// reads run outside the crypto lock; each `emit` locks only for CPU
    /// encrypt and releases before its POST, so a slow `post_envelope` cannot
    /// stall live delivery.
    ///
    /// `more` is true when a paginated (`after`/`before`) query returned exactly
    /// `limit` rows (more may remain pullable); the `latest` snapshot mode is
    /// always `more=false`. `count` is the rows actually emitted (decode
    /// failures are skipped, not counted).
    async fn handle_history(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        // req_id-aware panic boundary (mirrors `handle_agent_op`): a panic in
        // the batch is contained and surfaces as an empty `HistoryBatchEnd` so
        // the app is not left waiting on its deadline; it retries on reconnect.
        if AssertUnwindSafe(self.handle_history_inner(trace, req_id, after, before, limit))
            .catch_unwind()
            .await
            .is_err()
        {
            warn!(req_id, "history batch panicked; emitting empty marker");
            if let Err(e) = self
                .emit(
                    trace,
                    TagmaReply::HistoryBatchEnd {
                        req_id,
                        count: 0,
                        more: false,
                    },
                    None,
                )
                .await
            {
                warn!(req_id, "history panic-marker emit failed: {e:#}");
            }
        }
    }

    async fn handle_history_inner(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        let limit = limit.min(Self::HISTORY_BATCH_MAX);
        // Read + decode through the external projector, the single history read
        // path shared with the direct `/external/history` endpoint. Returns
        // decoded replies + the `more` flag.
        let Some(projector) = self
            .inner
            .state
            .upgrade()
            .and_then(|s| s.external.get().cloned())
        else {
            // Tagma shutting down: nothing to emit.
            return;
        };
        let (replies, more) = projector.read_history(after, before, limit).await;
        let mut count = 0u32;
        for reply in replies {
            if let Err(e) = self.emit(trace, reply, None).await {
                warn!(
                    req_id,
                    "history emit failed: {e:#}; stopping batch (no marker)"
                );
                // Deliberately do NOT emit HistoryBatchEnd: partial delivery
                // means the app's cursor has not advanced past the un-delivered
                // rows, and the next reconnect re-requests from `maxRendered`.
                return;
            }
            count += 1;
        }
        if let Err(e) = self
            .emit(
                trace,
                TagmaReply::HistoryBatchEnd {
                    req_id,
                    count,
                    more,
                },
                None,
            )
            .await
        {
            warn!(req_id, "history batch-end emit failed: {e:#}");
        }
    }

    /// Translate one op into an in-process tagma call against the root agent and
    /// produce the matching reply. (The former standalone connector did this
    /// over HTTP via `TagmaClient`; the fold calls the registry directly.)
    ///
    /// This runs under the `catch_unwind` boundary in `handle_user_op`, so it
    /// must stay unwind-safe: no non-`UnwindSafe` guard (e.g. a `MutexGuard`)
    /// may be held across a `.await`. `deliver_message`/`interrupt_root` scope
    /// their registry guards to synchronous blocks and release them before any
    /// await — keep it that way if you extend them.
    async fn execute_op(&self, request: TagmaRequest) -> Result<TagmaReply> {
        let state = self
            .inner
            .state
            .upgrade()
            .context("tagma shutting down; relay op dropped")?;
        match request {
            TagmaRequest::SendMessage { req_id, text } => {
                let resp = crate::routes::deliver_message(
                    &state,
                    Identity::Operator,
                    &self.inner.root_agent,
                    &text,
                )
                .await?;
                Ok(TagmaReply::MessageAccepted {
                    req_id,
                    queue_depth: resp.queue_depth,
                    warning: resp.warning,
                    // Stamped by `handle_agent_op` with the inbound row id; 0
                    // until then (and when no history store is configured).
                    history_id: 0,
                    // Stamped by `handle_agent_op` with the inbound row's
                    // created_at; absent until then.
                    created_at: None,
                })
            }
            TagmaRequest::Interrupt { req_id } => {
                self.interrupt_root(&state).await?;
                Ok(TagmaReply::Interrupted { req_id })
            }
        }
    }

    /// Cancel the root agent's current round (interrupt), mirroring the
    /// `interrupt_agent` route in-process. No auth needed — the relay is the
    /// trusted operator-equivalent.
    async fn interrupt_root(&self, state: &SharedState) -> Result<()> {
        let round_cancel = {
            let registry = state.registry.read().await;
            let Some(entry) = registry.get(&self.inner.root_agent) else {
                anyhow::bail!("root agent not found for interrupt");
            };
            let live = entry
                .as_live()
                .context("root agent is faulted; nothing to interrupt")?;
            live.agent.round_cancel.clone()
        };
        if let Some(round) = round_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            round.cancel();
        }
        Ok(())
    }

    /// Push a runtime signal (busy/idle presence, turn terminals, errors) to
    /// the relay for plaintext rebroadcast as a `LescheEvent::TagmaSignal`.
    /// Signals are operator metadata: they do NOT enter the encrypted envelope
    /// channel and are NOT persisted in `chat_history` (a reconnect only
    /// replays authored messages). Best-effort and not retried, mirroring
    /// [`post_status`](kallip_lesche_client::LescheClient::post_status): a
    /// signal is a transient transition that the next event supersedes, so a
    /// dropped POST just means the UI misses one frame of presence, and the
    /// projector has already logged it for observability. `cancel` aborts an
    /// in-flight POST on re-KEX/shutdown.
    async fn emit_signal(
        &self,
        event: kallip_common::protocol::SignalEvent,
        cancel: Option<&CancellationToken>,
    ) -> Result<()> {
        let post = self.inner.client.post_signal(&self.inner.tagma_id, &event);
        match cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => Ok(()),
                r = post => r,
            },
            None => post.await,
        }
    }

    /// Encrypt `reply` for the conversation and post the agent envelope. Returns
    /// `Err` when delivery is exhausted (the pump logs and carries on; app
    /// recovery is via host-history re-pull on reconnect). `cancel`, when set
    /// (the pump path), aborts an in-flight POST on re-KEX/shutdown instead of
    /// waiting out its 30 s timeout.
    async fn emit(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        reply: TagmaReply,
        cancel: Option<&CancellationToken>,
    ) -> Result<()> {
        // Serialize + encrypt under the crypto lock: the non-Copy SessionKey
        // never leaves the lock scope (no clone, no copied local). Both
        // `to_vec` and `encrypt` are CPU-only with no await, so the Mutex is
        // not held across a suspension. Do not move them back out — that would
        // require copying the key out of the lock.
        let envelope = {
            let mut c = self.inner.crypto.lock().await;
            if c.key.is_none() {
                // No session key yet: the app is not connected. Drop silently;
                // the pump re-emits live events once a KEX lands.
                return Ok(());
            }
            // Mutate the counter before borrowing the key, so the immutable key
            // borrow (held across `encrypt`) does not overlap a mutable access.
            let seq = c.outbound_seq;
            c.outbound_seq += 1;
            let key = c.key.as_ref().expect("checked non-empty above");
            let json = serde_json::to_vec(&reply).context("encode tagma reply")?;
            let ciphertext = e2e::encrypt(key, seq, &json);
            Envelope {
                conversation_id: self.inner.conversation_id.clone(),
                sender: Participant::Agent {
                    tagma_id: self.inner.tagma_id.clone(),
                },
                sequence_n: seq,
                trace_id: trace.clone(),
                timestamp: OffsetDateTime::now_utc(),
                ciphertext: Ciphertext(ciphertext),
            }
        };
        // Deliberately do NOT roll back `outbound_seq` on POST failure — the seq
        // is mixed into the AEAD nonce, and a POST can fail after the relay has
        // accepted/forwarded the envelope, so reusing the seq would risk a
        // nonce reuse under the same epoch key. Burning a gap is safe: the app
        // applies envelopes by decryption, not sequence validation. Aborting the
        // POST on `cancel` (re-KEX / shutdown) likewise just burns the gap.
        let post = self
            .inner
            .client
            .post_envelope(&self.inner.conversation_id, &envelope);
        match cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => Ok(()),
                r = post => r,
            },
            None => post.await,
        }
    }
}

#[cfg(test)]
mod op_tests {
    //! Operation-level tests for the in-process relay: a mock lesche that
    //! captures posted envelopes, driven by a real `AppState` with a minimal
    //! root agent. The initiator side is simulated inline (dir-0 encrypt of the
    //! request, dir-1 decrypt of the replies). This proves the semantic channel
    //! — encrypt -> relay op -> in-process tagma call -> encrypt reply -> decrypt
    //! — without the real agora or any TS. Adapted from the former standalone
    //! connector's HTTP-mock-tagma tests, now exercising `deliver_message` and
    //! the broadcast pump directly.

    use super::*;
    use axum::extract::State;
    use axum::{Router, routing::post};
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
    use kallip_agora_common::bytes::Ciphertext;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
    use kallip_common::protocol::{AuthoredEvent, FailoverChainExhaustion, SignalEvent, SseEvent};
    use kallip_e2ee::{
        DIR_INITIATOR_TO_RESPONDER, DIR_RESPONDER_TO_INITIATOR, DeviceKey, SessionKey, nonce,
    };
    use kallip_lesche_common::control::KeyExchangeInit;
    use kallip_lesche_common::message::{Envelope, Participant, TagmaReply, TagmaRequest};
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::{Mutex, broadcast, mpsc};

    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry_with_rx, make_state};

    /// Captured outbound envelopes, in arrival order.
    type Capture = Arc<Mutex<Vec<Envelope>>>;

    /// Captured system signals (busy/idle/terminals/errors), in arrival order.
    type SignalCapture = Arc<Mutex<Vec<SignalEvent>>>;

    /// Initiator-side encrypt (direction 0 = initiator->responder). The e2e
    /// crate's `encrypt` is hardcoded to the responder's direction (1), so the
    /// test initiator side builds the AEAD with an explicit direction via the
    /// shared `nonce` + `DIR_*`.
    fn initiator_encrypt(key: &[u8; 32], seq: u64, pt: &[u8]) -> Vec<u8> {
        let aead = ChaCha20Poly1305::new(key.into());
        aead.encrypt(&Nonce::from(nonce(DIR_INITIATOR_TO_RESPONDER, seq)), pt)
            .unwrap()
    }

    /// Initiator-side decrypt (direction 1 = responder->initiator).
    fn initiator_decrypt(key: &[u8; 32], seq: u64, ct: &[u8]) -> Option<Vec<u8>> {
        let aead = ChaCha20Poly1305::new(key.into());
        aead.decrypt(&Nonce::from(nonce(DIR_RESPONDER_TO_INITIATOR, seq)), ct)
            .ok()
    }

    async fn spawn_lesche(capture: Capture, signals: SignalCapture) -> String {
        let app = Router::new()
            .route("/v1/conversations/{conv}/envelopes", post(capture_handler))
            .route("/v1/tagmata/{_tagma}/signal", post(signal_handler))
            .route("/v1/tagmata/{_tagma}/status", post(|| async { "ok" }))
            .with_state((capture, signals));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn capture_handler(
        State((c, _)): State<(Capture, SignalCapture)>,
        env: axum::Json<Envelope>,
    ) -> &'static str {
        c.lock().await.push(env.0);
        "ok"
    }

    async fn signal_handler(
        State((_, s)): State<(Capture, SignalCapture)>,
        axum::Json(event): axum::Json<SignalEvent>,
    ) -> &'static str {
        s.lock().await.push(event);
        "ok"
    }

    /// Build a relay wired to a fresh mock lesche and a real `AppState` whose
    /// single root agent has a capturable prompt channel and a `events_cap`-deep
    /// event buffer. A pre-shared session key is installed (KEX itself is
    /// covered by e2e tests). Returns the handle, the key, the envelope capture,
    /// the root's prompt receiver, the root id, and the state.
    async fn setup(
        events_cap: usize,
    ) -> (
        RelayHandle,
        SessionKey,
        Capture,
        mpsc::Receiver<String>,
        AgentId,
        SharedState,
    ) {
        let (handle, key, capture, _signals, prompt_rx, root_id, state, _db) =
            setup_inner(events_cap, None).await;
        (handle, key, capture, prompt_rx, root_id, state)
    }

    /// Like [`setup`] but also exposes the captured system signals (for tests
    /// that assert on the plaintext signal channel).
    async fn setup_with_signals(
        events_cap: usize,
    ) -> (
        RelayHandle,
        SessionKey,
        Capture,
        SignalCapture,
        mpsc::Receiver<String>,
        AgentId,
        SharedState,
    ) {
        let (handle, key, capture, signals, prompt_rx, root_id, state, _db) =
            setup_inner(events_cap, None).await;
        (handle, key, capture, signals, prompt_rx, root_id, state)
    }

    /// Like [`setup`] but with a real tempfile-backed chat-history store shared
    /// with the projector, for `TagmaControl::History` replay tests. Returns the
    /// `Db` (so tests can append rows the projector will read) and the `TempDir`
    /// (to keep the file alive for the test's duration).
    async fn setup_with_history(
        events_cap: usize,
    ) -> (
        RelayHandle,
        SessionKey,
        Capture,
        mpsc::Receiver<String>,
        AgentId,
        SharedState,
        chat_history::Db,
        TempDir,
    ) {
        let dir = TempDir::new().unwrap();
        let db = chat_history::open(&dir.path().join("history.sqlite"))
            .await
            .unwrap();
        let (handle, key, capture, _signals, prompt_rx, root_id, state, db) =
            setup_inner(events_cap, Some(db.clone())).await;
        (
            handle,
            key,
            capture,
            prompt_rx,
            root_id,
            state,
            db.unwrap(),
            dir,
        )
    }

    async fn setup_inner(
        events_cap: usize,
        history: Option<chat_history::Db>,
    ) -> (
        RelayHandle,
        SessionKey,
        Capture,
        SignalCapture,
        mpsc::Receiver<String>,
        AgentId,
        SharedState,
        Option<chat_history::Db>,
    ) {
        let state = make_state();
        let root_id = AgentId::from("root".to_string());
        let (mut entry, prompt_rx) = make_entry_with_rx(None, "root-tok".to_string());
        // Give the pump enough buffer that a burst of sends does not overflow
        // before the spawned pump task drains.
        let (events_tx, _) = broadcast::channel(events_cap);
        entry.agent.events_tx = events_tx;
        {
            let mut registry = state.registry.write().await;
            registry
                .register_root(root_id.clone(), RegistryEntry::Live(entry))
                .expect("register root");
        }

        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let signals: SignalCapture = Arc::new(Mutex::new(Vec::new()));
        let lesche_url = spawn_lesche(capture.clone(), signals.clone()).await;
        let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
        let device = DeviceKey::generate();
        let tagma_id = TagmaId::from("tagma".to_string());
        let conversation_id = ConversationId::for_tagma(&tagma_id);
        // Install the external projector with the (optional) history store +
        // conversation id, so the relay's history reads (handle_history) and
        // inbound persistence (deliver_message → record_inbound) go through the
        // same sole-writer the production boot installs.
        let projector = crate::external::ExternalProjector::new(
            Arc::downgrade(&state),
            history.clone(),
            Some(conversation_id),
            MessageLimits::default(),
        );
        if state.external.set(projector).is_err() {
            panic!("projector must be installed once per test state");
        }
        let handle = RelayHandle::new(
            client,
            tagma_id,
            device,
            root_id.clone(),
            Arc::downgrade(&state),
        );
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("getrandom");
        let key = SessionKey::new(key);
        handle.inner.crypto.lock().await.key = Some(key.clone());
        (
            handle, key, capture, signals, prompt_rx, root_id, state, history,
        )
    }

    /// Encrypt an app->tagma payload (a serialized `TagmaRequest`) into a
    /// user-sender envelope, dir 0 (initiator->responder).
    fn payload_envelope(
        key: &SessionKey,
        conv: &ConversationId,
        seq: u64,
        bytes: &[u8],
    ) -> Envelope {
        Envelope {
            conversation_id: conv.clone(),
            sender: Participant::User {
                user_id: UserId::from("u".to_string()),
            },
            sequence_n: seq,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(initiator_encrypt(key, seq, bytes)),
        }
    }

    fn user_envelope(
        key: &SessionKey,
        conv: &ConversationId,
        seq: u64,
        request: TagmaRequest,
    ) -> Envelope {
        let bytes = serde_json::to_vec(&request).unwrap();
        payload_envelope(key, conv, seq, &bytes)
    }

    /// Decrypt the captured envelopes into replies.
    async fn drain_replies(capture: &Capture, key: &SessionKey) -> Vec<TagmaReply> {
        capture
            .lock()
            .await
            .clone()
            .into_iter()
            .map(|env| {
                let plain = initiator_decrypt(key, env.sequence_n, &env.ciphertext.0).unwrap();
                serde_json::from_slice::<TagmaReply>(&plain).unwrap()
            })
            .collect()
    }

    /// Resolve the relay's conversation id (derived from the tagma id).
    fn conv_of(handle: &RelayHandle) -> ConversationId {
        handle.inner.conversation_id.clone()
    }

    #[tokio::test]
    async fn send_message_round_trips() {
        let (handle, key, capture, mut prompt_rx, _root_id, _state) = setup(1).await;
        let conv = conv_of(&handle);
        handle
            .handle_user_op(user_envelope(
                &key,
                &conv,
                1,
                TagmaRequest::SendMessage {
                    req_id: 10,
                    text: "hello".into(),
                },
            ))
            .await;
        // The root agent's prompt channel received the text (with the
        // `[From: operator]` header deliver_message attaches).
        let delivered = prompt_rx.recv().await.expect("message delivered");
        assert!(delivered.contains("hello"), "delivered: {delivered}");
        // The app got a MessageAccepted reply.
        let replies = drain_replies(&capture, &key).await;
        assert!(matches!(
            replies.as_slice(),
            [TagmaReply::MessageAccepted {
                req_id: 10,
                queue_depth: 0,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn interrupt_round_trips() {
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        let conv = conv_of(&handle);
        handle
            .handle_user_op(user_envelope(
                &key,
                &conv,
                1,
                TagmaRequest::Interrupt { req_id: 7 },
            ))
            .await;
        // interrupt_root is a no-op against the minimal root (no active round
        // token); the relay still emits the Interrupted ack.
        let replies = drain_replies(&capture, &key).await;
        assert!(matches!(
            replies.as_slice(),
            [TagmaReply::Interrupted { req_id: 7 }]
        ));
    }

    #[tokio::test]
    async fn op_before_key_exchange_is_dropped() {
        // A relay with no session key must drop the op silently (no in-process
        // call, no reply).
        let (handle, _key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        // Wipe the installed key so the epoch is empty.
        handle.inner.crypto.lock().await.key = None;
        let conv = conv_of(&handle);
        handle
            .handle_user_op(user_envelope(
                &key_zero(),
                &conv,
                1,
                TagmaRequest::SendMessage {
                    req_id: 1,
                    text: "x".into(),
                },
            ))
            .await;
        assert!(capture.lock().await.is_empty(), "no reply before KEX");
    }

    /// The first message of a crypto epoch carries `sequence_n = 0` and MUST be
    /// accepted (a plain `u64` window initialized to 0 would reject it as
    /// `0 <= 0`). The window is `None` until the first message lands, so seq=0
    /// passes; the same `None` state is restored on every KEX reset.
    #[tokio::test]
    async fn first_inbound_seq_zero_of_an_epoch_is_accepted() {
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        let conv = conv_of(&handle);
        handle
            .handle_user_op(user_envelope(
                &key,
                &conv,
                0,
                TagmaRequest::SendMessage {
                    req_id: 1,
                    text: "first of epoch".into(),
                },
            ))
            .await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(
            replies.len(),
            1,
            "the first seq=0 of an epoch must be accepted and produce a reply"
        );
    }

    #[tokio::test]
    async fn replayed_inbound_envelope_is_dropped() {
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        let conv = conv_of(&handle);
        let env = user_envelope(
            &key,
            &conv,
            5,
            TagmaRequest::SendMessage {
                req_id: 1,
                text: "first".into(),
            },
        );
        handle.handle_user_op(env.clone()).await;
        // A replay of the same sequence number is dropped without a second reply.
        handle.handle_user_op(env).await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(
            replies.len(),
            1,
            "replayed seq must not produce a second reply"
        );
    }

    #[tokio::test]
    async fn garbage_ciphertext_does_not_advance_replay_window() {
        // A forged envelope with a huge `sequence_n` and undecryptable
        // ciphertext must NOT advance the replay high-water-mark. If it did,
        // every later legitimate envelope (seq < u64::MAX) would be silently
        // dropped as a replay for the rest of the epoch — a one-shot blind.
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        let conv = conv_of(&handle);
        let forged = Envelope {
            conversation_id: conv.clone(),
            sender: Participant::User {
                user_id: UserId::from("u".to_string()),
            },
            sequence_n: u64::MAX,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        };
        handle.handle_user_op(forged).await;
        // A normal legitimate envelope must still produce a reply.
        handle
            .handle_user_op(user_envelope(
                &key,
                &conv,
                1,
                TagmaRequest::SendMessage {
                    req_id: 1,
                    text: "after forge".into(),
                },
            ))
            .await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(
            replies.len(),
            1,
            "undecryptable envelope must not poison the replay window"
        );
    }

    #[tokio::test]
    async fn pump_splits_authored_envelope_and_system_signal() {
        let events = vec![
            SseEvent::Busy,
            SseEvent::AssistantContent {
                content: "hi".into(),
            },
            SseEvent::Idle,
            SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into(),
            }, // dropped (out of capability)
        ];
        let (handle, key, capture, signals, _prompt_rx, _root_id, state) =
            setup_with_signals(16).await;
        handle.start_pump().await;

        // Resolve the root's event sender and wait for the pump to subscribe
        // (broadcast::send with no receiver returns Err).
        let events_tx = {
            let registry = state.registry.read().await;
            let (_, entry) = registry.root_agent().expect("root present");
            entry.as_live().expect("root live").agent.events_tx.clone()
        };
        for _ in 0..400 {
            if events_tx.receiver_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for ev in events {
            events_tx.send(ev).expect("pump subscribed");
            // Yield between sends so the pump's recv->emit loop keeps up; the
            // 16-deep buffer makes this a safety margin, not a hard gate.
            tokio::task::yield_now().await;
        }

        // Authored content rides the encrypted envelope channel; busy/idle do
        // NOT (they cross as plaintext signals), so the envelope capture holds
        // exactly one authored reply.
        let mut got = Vec::new();
        for _ in 0..300 {
            got = drain_replies(&capture, &key).await;
            if !got.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // System signals ride the plaintext channel; drain them too.
        let mut sig = Vec::new();
        for _ in 0..300 {
            sig = signals.lock().await.clone();
            if sig.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.stop_pump().await;
        assert_eq!(
            got.len(),
            1,
            "only authored content (AssistantContent) rides the envelope channel"
        );
        assert!(matches!(
            got[0],
            TagmaReply::Event {
                event: AuthoredEvent::AssistantContent { .. },
                ..
            }
        ));
        assert_eq!(sig.len(), 2, "busy/idle cross as plaintext system signals");
        assert!(matches!(sig[0], SignalEvent::Busy));
        assert!(matches!(sig[1], SignalEvent::Idle));
    }

    #[test]
    fn project_external_splits_authored_and_signal() {
        use crate::projector::project_external;
        // Authored content -> authored half only.
        assert!(matches!(
            project_external(&SseEvent::AssistantContent {
                content: "c".into()
            }),
            (Some(AuthoredEvent::AssistantContent { .. }), None)
        ));
        // System signals -> signal half only, no authored, never persisted.
        assert!(matches!(
            project_external(&SseEvent::Busy),
            (None, Some(SignalEvent::Busy))
        ));
        assert!(matches!(
            project_external(&SseEvent::Interrupted),
            (None, Some(SignalEvent::Interrupted))
        ));
        assert!(matches!(
            project_external(&SseEvent::Cancelled),
            (None, Some(SignalEvent::Cancelled))
        ));
        assert!(matches!(
            project_external(&SseEvent::MaxRoundsExceeded),
            (None, Some(SignalEvent::MaxRoundsExceeded))
        ));
        assert!(matches!(
            project_external(&SseEvent::TokenBudgetExceeded {
                consumed: 1,
                budget: 2
            }),
            (
                None,
                Some(SignalEvent::TokenBudgetExceeded {
                    consumed: 1,
                    budget: 2
                })
            )
        ));
        assert!(matches!(
            project_external(&SseEvent::Error {
                message: "m".into()
            }),
            (None, Some(SignalEvent::Error { .. }))
        ));
        {
            let (a, s) = project_external(&SseEvent::Error {
                message: "m".into(),
            });
            assert!(a.is_none());
            match s {
                Some(SignalEvent::Error { message }) => assert_eq!(message, "m"),
                other => panic!("expected Error, got {other:?}"),
            }
        }
        {
            let (a, s) = project_external(&SseEvent::FailoverChainExhausted {
                reason: FailoverChainExhaustion::NoFailoverConfigured,
                detail: "d".into(),
            });
            assert!(a.is_none());
            match s {
                Some(SignalEvent::FailoverChainExhausted { reason, detail }) => {
                    assert!(matches!(
                        reason,
                        FailoverChainExhaustion::NoFailoverConfigured
                    ));
                    assert_eq!(detail, "d");
                }
                other => panic!("expected FailoverChainExhausted, got {other:?}"),
            }
        }
        // Dropped (out-of-capability) variants map to neither half.
        assert!(matches!(
            project_external(&SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into()
            }),
            (None, None)
        ));
        assert!(matches!(
            project_external(&SseEvent::AssistantContentDelta { delta: "d".into() }),
            (None, None)
        ));
        assert!(matches!(
            project_external(&SseEvent::ApprovalUpdated {
                id: "a".into(),
                status: kallip_common::approval::ApprovalStatus::Pending
            }),
            (None, None)
        ));
    }

    #[tokio::test]
    async fn re_kex_installs_key_resets_seq_and_starts_pump() {
        // Advance the outbound counter, then a KEX must reset it to 0 and leave
        // a session key installed + a pump running.
        let (handle, _key, _capture, _prompt_rx, _root_id, _state) = setup(1).await;
        {
            let mut c = handle.inner.crypto.lock().await;
            c.outbound_seq = 42;
            c.seen_inbound = Some(42);
        }

        // App side: a real ephemeral keypair so respond_key_exchange succeeds.
        let app_secret = x25519_dalek::ReusableSecret::random();
        let app_pub = x25519_dalek::PublicKey::from(&app_secret);
        let init = KeyExchangeInit {
            ephemeral_public: kallip_agora_common::bytes::X25519PublicKey(
                app_pub.to_bytes().to_vec(),
            ),
        };
        handle.handle_kex(conv_of(&handle), init).await;

        let c = handle.inner.crypto.lock().await;
        assert!(c.key.is_some(), "KEX must install a session key");
        assert_eq!(c.outbound_seq, 0, "KEX must reset the outbound counter");
        assert_eq!(c.seen_inbound, None, "KEX must reset the inbound window");
        drop(c);
        assert!(
            handle.inner.pump.lock().await.is_some(),
            "KEX must start the pump"
        );
        handle.stop_pump().await;
    }

    /// A zero key for the no-KEX drop test (the ciphertext never decrypts; the
    /// op is dropped at the key-absent check before decryption anyway).
    fn key_zero() -> SessionKey {
        SessionKey::new([0u8; 32])
    }

    /// `handle_user_op` with `SendMessage` drives the real `deliver_message`,
    /// which calls the projector's `record_inbound`: the inbound row is
    /// persisted once under the conversation id. The `MessageAccepted` ack
    /// therefore carries `history_id = 0` (the app dedups its optimistic line
    /// off the `UserMessage` frame the projector publishes and the pump
    /// forwards — exercised in `pump_splits_authored_envelope_and_system_signal`).
    /// The row then replays as a `UserMessage` echo under its row id.
    #[tokio::test]
    async fn send_message_persists_inbound_and_forwards_usermessage() {
        let (handle, key, capture, mut prompt_rx, _root_id, _state, db, _dir) =
            setup_with_history(8).await;
        let conv = conv_of(&handle);
        handle
            .handle_user_op(user_envelope(
                &key,
                &conv,
                1,
                TagmaRequest::SendMessage {
                    req_id: 10,
                    text: "hi".into(),
                },
            ))
            .await;
        // The root agent received the prompt.
        let _ = prompt_rx.recv().await.expect("message delivered");
        let replies = drain_replies(&capture, &key).await;

        // The ack is present and carries history_id = 0: the inbound row id no
        // longer rides the ack (the projector-published UserMessage frame is the
        // promotion path).
        let ack = replies.iter().find_map(|r| match r {
            TagmaReply::MessageAccepted {
                req_id: 10,
                history_id,
                ..
            } => Some(*history_id),
            _ => None,
        });
        assert_eq!(
            ack,
            Some(0),
            "MessageAccepted ack no longer stamps history_id"
        );

        // The inbound row is persisted under the conversation id.
        let rows = chat_history::read_last_n(&db, conv.as_ref(), 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].direction, "inbound");
        let um_id = rows[0].id;
        assert!(um_id > 0, "inbound row assigned a positive id");

        // It replays as a UserMessage echo under its row id.
        capture.lock().await.clear();
        let trace = kallip_agora_common::ids::TraceId::from("h".to_string());
        handle.handle_history(&trace, 1, None, None, 50).await;
        let replies = drain_replies(&capture, &key).await;
        let um = replies.iter().find_map(|r| match r {
            TagmaReply::UserMessage {
                history_id, text, ..
            } if *history_id == um_id => Some(text),
            _ => None,
        });
        assert_eq!(
            um,
            Some(&"hi".to_string()),
            "inbound row echoed as UserMessage"
        );
    }

    /// `handle_history` (latest mode) replays both outbound and inbound rows in
    /// id order: outbound as its stored `Event` reply, inbound as a `UserMessage`
    /// echo, each stamped with its row id, then a `HistoryBatchEnd` marker.
    #[tokio::test]
    async fn handle_history_latest_replays_both_directions_in_order() {
        let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
            setup_with_history(8).await;
        let conv = conv_of(&handle);
        let trace = kallip_agora_common::ids::TraceId::from("test".to_string());
        // Outbound, inbound, outbound — interleaved, ids assigned in append order.
        // Outbound rows are written directly (the projector is the production
        // writer; here we seed the shared store the projector reads from).
        let mut o0 = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "o0".into(),
            },
            history_id: 0,
            created_at: None,
        };
        chat_history::stamp_reply(&db, conv.as_ref(), &mut o0).await;
        chat_history::append(
            &db,
            conv.as_ref(),
            "inbound",
            "send_message",
            &serde_json::to_vec(&TagmaRequest::SendMessage {
                req_id: 1,
                text: "u0".into(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let mut o1 = TagmaReply::Event {
            event: AuthoredEvent::AssistantContent {
                content: "o1".into(),
            },
            history_id: 0,
            created_at: None,
        };
        chat_history::stamp_reply(&db, conv.as_ref(), &mut o1).await;

        capture.lock().await.clear();
        handle.handle_history(&trace, 7, None, None, 50).await;

        let replies = drain_replies(&capture, &key).await;
        // o0, u0, o1, batch-end = 4 frames.
        assert_eq!(replies.len(), 4);
        let mut last = 0i64;
        let mut saw_end = false;
        for r in &replies {
            match r {
                TagmaReply::Event { history_id, .. }
                | TagmaReply::UserMessage { history_id, .. } => {
                    assert!(
                        *history_id > last,
                        "ids must strictly increase across the batch"
                    );
                    last = *history_id;
                }
                TagmaReply::HistoryBatchEnd {
                    req_id,
                    count,
                    more,
                } => {
                    assert_eq!(*req_id, 7);
                    assert_eq!(*count, 3);
                    assert!(!more);
                    saw_end = true;
                }
                other => panic!("unexpected reply {other:?}"),
            }
        }
        assert!(saw_end, "batch must end with a HistoryBatchEnd marker");
    }

    /// `handle_history` (latest mode) reports `more=false` even when the stored
    /// row count equals the (capped) request limit: latest is a recent-N
    /// snapshot with no further page to pull, so a full page must NOT advertise
    /// more. (Guards against the `rows.len() == limit` heuristic leaking into
    /// the latest branch.)
    #[tokio::test]
    async fn handle_history_latest_more_is_false_even_at_full_page() {
        let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
            setup_with_history(8).await;
        let conv = conv_of(&handle);
        let trace = kallip_agora_common::ids::TraceId::from("test".to_string());
        // Insert exactly `limit` rows.
        for i in 0..3 {
            chat_history::append(
                &db,
                conv.as_ref(),
                "outbound",
                "event",
                &serde_json::to_vec(&TagmaReply::Event {
                    event: AuthoredEvent::AssistantContent {
                        content: format!("e{i}"),
                    },
                    history_id: 0,
                    created_at: None,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        }

        capture.lock().await.clear();
        // Request exactly 3 (== stored count); latest mode.
        handle.handle_history(&trace, 9, None, None, 3).await;

        let replies = drain_replies(&capture, &key).await;
        match replies.last().expect("batch end present") {
            TagmaReply::HistoryBatchEnd { count, more, .. } => {
                assert_eq!(*count, 3);
                assert!(
                    !more,
                    "latest mode must never advertise more, even at a full page"
                );
            }
            other => panic!("expected batch end, got {other:?}"),
        }
    }

    /// `handle_history` in `after` mode returns only rows with id > after
    /// (incremental catch-up); `before` mode returns the older chunk and sets
    /// `more` when truncated by `limit`.
    #[tokio::test]
    async fn handle_history_after_and_before_windows() {
        let (handle, key, capture, _prompt_rx, _root_id, _state, db, _dir) =
            setup_with_history(8).await;
        let conv = conv_of(&handle);
        let mut ids = Vec::new();
        for i in 0..4 {
            ids.push(
                chat_history::append(
                    &db,
                    conv.as_ref(),
                    "outbound",
                    "event",
                    &serde_json::to_vec(&TagmaReply::Event {
                        event: AuthoredEvent::AssistantContent {
                            content: format!("e{i}"),
                        },
                        history_id: 0,
                        created_at: None,
                    })
                    .unwrap(),
                )
                .await
                .unwrap()
                .0,
            );
        }
        let trace = kallip_agora_common::ids::TraceId::from("test".to_string());

        // after=ids[0] -> ids[1..3] + batch-end; more=false (3 < 50).
        capture.lock().await.clear();
        handle
            .handle_history(&trace, 1, Some(ids[0]), None, 50)
            .await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(replies.len(), 4, "after-window: 3 rows + end");
        match &replies[0] {
            TagmaReply::Event { history_id, .. } => assert_eq!(*history_id, ids[1]),
            other => panic!("{other:?}"),
        }

        // before=ids[3] limit 2 -> ids[1], ids[2] + batch-end; more=true (hit limit).
        capture.lock().await.clear();
        handle
            .handle_history(&trace, 2, None, Some(ids[3]), 2)
            .await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(replies.len(), 3, "before-window: 2 rows + end");
        match replies.last().unwrap() {
            TagmaReply::HistoryBatchEnd { count, more, .. } => {
                assert_eq!(*count, 2);
                assert!(more, "more must be true when the limit is hit");
            }
            other => panic!("expected batch end, got {other:?}"),
        }
    }

    /// With no history store configured, `handle_history` emits an empty
    /// `HistoryBatchEnd` so the app is not left waiting on its deadline.
    #[tokio::test]
    async fn handle_history_without_store_emits_empty_batch_end() {
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(8).await;
        let trace = kallip_agora_common::ids::TraceId::from("h".to_string());
        handle.handle_history(&trace, 5, None, None, 50).await;
        let replies = drain_replies(&capture, &key).await;
        assert!(matches!(
            replies.as_slice(),
            [TagmaReply::HistoryBatchEnd {
                req_id: 5,
                count: 0,
                more: false
            }]
        ));
    }
}
