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
//! - pump the root agent's `SseEvent` stream onto the agent-free `TagmaEvent`
//!   vocabulary and post encrypted envelopes (`pump`);
//! - deliver the agent's `kallip lesche send` text as an `AssistantContent`
//!   envelope (`emit_message`).
//!
//! The E2E key never leaves this process. `Inner` holds a `Weak<AppState>` (not
//! a strong ref) to avoid a reference cycle with `AppState.relay`.

mod crypto;
mod ops;
mod pump;

use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::event::TagmaEvent;
use kallip_agora_common::ids::{ConversationId, TagmaId};
use kallip_agora_common::message::{Envelope, Participant, TagmaReply, TagmaRequest};
use kallip_agora_common::tunnel::TunnelInbound;
use kallip_e2ee::{self as e2e, DeviceKey};
use kallip_lesche_client::LescheClient;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use kallip_common::agentid::AgentId;

use crate::auth::Identity;
use crate::state::{AppState, SharedState};

use crypto::CryptoState;
use ops::{MessageLimiter, op_err_reply, op_trace, req_id_of};

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
    /// In-flight per-envelope op tasks, so shutdown can abort and drain them
    /// rather than leaving them fire-and-forget. See [`RelayHandle::stop_dispatch`].
    dispatch: Mutex<tokio::task::JoinSet<()>>,
    /// `Weak` to break the `RelayHandle` ↔ `AppState` reference cycle. Upgraded
    /// at call time; `None` during shutdown → the op fails gracefully.
    state: Weak<AppState>,
    /// Process-global message burst limiter.
    message_limiter: Mutex<MessageLimiter>,
}

impl RelayHandle {
    pub fn new(
        client: LescheClient,
        tagma_id: TagmaId,
        device: DeviceKey,
        root_agent: AgentId,
        message_limits: MessageLimits,
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
                dispatch: Mutex::new(tokio::task::JoinSet::new()),
                state,
                message_limiter: Mutex::new(MessageLimiter::new(message_limits)),
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
        self.stop_dispatch().await;
    }

    /// Abort and reap all in-flight op-dispatch tasks. Only safe on a process-
    /// tearing-down path: `deliver_message`'s Phase 2 (spawn-agent → install) is
    /// not abort-safe mid-flight, so aborting a dispatch there can leak spawned
    /// tasks or leave a disarmed workspace lock. Both `run` shutdown branches
    /// are process-exit paths, so this is acceptable; a future non-shutdown
    /// caller must first make `deliver_message` abort-safe.
    async fn stop_dispatch(&self) {
        let mut set = self.inner.dispatch.lock().await;
        set.abort_all();
        while set.join_next().await.is_some() {}
    }

    /// Open the tunnel SSE and dispatch each inbound message (each on its own
    /// task so a long-running op does not stall the stream reader).
    async fn connect_and_drain(self) -> Result<()> {
        let stream = self
            .inner
            .client
            .open_tunnel(&self.inner.device, &self.inner.tagma_id)
            .await?;
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
        Ok(())
    }

    async fn dispatch(self, inbound: TunnelInbound) {
        match inbound {
            TunnelInbound::KeyExchange {
                conversation_id,
                init,
            } => self.handle_kex(conversation_id, init).await,
            TunnelInbound::Envelope { envelope } => {
                // Outer last-resort: a panic before `req_id` is parsed cannot be
                // attributed to an op, so we can only log. The req_id-aware
                // recovery inside `handle_user_op` covers the common case.
                if AssertUnwindSafe(self.handle_user_op(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    warn!("relay op dispatch panicked before req_id was known");
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
        init: kallip_agora_common::control::KeyExchangeInit,
    ) {
        let (response, key) = match e2e::respond_key_exchange(
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
        {
            let mut c = self.inner.crypto.lock().await;
            c.key = Some(key);
            c.outbound_seq = 0;
            c.seen_inbound = None;
        }
        self.start_pump().await;
        if let Err(e) = self
            .inner
            .client
            .post_key_exchange_response(&conversation_id, &response)
            .await
        {
            warn!(conv = %conversation_id, "post key-exchange response: {e:#}");
        }
    }

    /// Decrypt an app op envelope, run it against the root agent, and emit the
    /// reply. The tagma call runs under a req_id-aware panic boundary so a bug
    /// never leaves the app hanging: a panic yields an `Error` reply for the
    /// exact `req_id`.
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
        let request: TagmaRequest = match serde_json::from_slice(&plain) {
            Ok(p) => p,
            Err(e) => {
                warn!("op decode failed: {e}");
                return;
            }
        };
        let req_id = req_id_of(&request);
        let trace = op_trace(req_id);

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
        if let Err(e) = self.emit(&trace, reply, None).await {
            warn!(req_id, "emit reply: {e:#}");
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

    /// Deliver the agent's `kallip lesche send` text to the user: gate on the
    /// process-global burst cap, then emit it as a `TagmaEvent::AssistantContent`
    /// envelope. The E2E key never leaves the process.
    pub async fn emit_message(&self, text: String) -> Result<(), RelayMessageError> {
        let allowed = self.inner.message_limiter.lock().await.check();
        if !allowed {
            return Err(RelayMessageError::BurstExceeded);
        }
        let trace = kallip_agora_common::ids::TraceId::from(ops::MESSAGE_TRACE.to_owned());
        self.emit(
            &trace,
            TagmaReply::Event {
                event: TagmaEvent::AssistantContent { content: text },
            },
            None,
        )
        .await?;
        Ok(())
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
    use kallip_agora_common::control::KeyExchangeInit;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
    use kallip_agora_common::message::{Envelope, Participant, TagmaReply, TagmaRequest};
    use kallip_common::protocol::SseEvent;
    use kallip_e2ee::{
        DIR_INITIATOR_TO_RESPONDER, DIR_RESPONDER_TO_INITIATOR, DeviceKey, SessionKey, nonce,
    };
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Mutex, broadcast, mpsc};

    use crate::state::RegistryEntry;
    use crate::test_helpers::{make_entry_with_rx, make_state};

    /// Captured outbound envelopes, in arrival order.
    type Capture = Arc<Mutex<Vec<Envelope>>>;

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

    async fn spawn_lesche(capture: Capture) -> String {
        let app = Router::new()
            .route("/v1/conversations/{conv}/envelopes", post(capture_handler))
            .with_state(capture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn capture_handler(State(c): State<Capture>, env: axum::Json<Envelope>) -> &'static str {
        c.lock().await.push(env.0);
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
        let lesche_url = spawn_lesche(capture.clone()).await;
        let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
        let device = DeviceKey::generate();
        let tagma_id = TagmaId::from("tagma".to_string());
        let handle = RelayHandle::new(
            client,
            tagma_id,
            device,
            root_id.clone(),
            MessageLimits::default(),
            Arc::downgrade(&state),
        );
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("getrandom");
        let key = SessionKey::new(key);
        handle.inner.crypto.lock().await.key = Some(key.clone());
        (handle, key, capture, prompt_rx, root_id, state)
    }

    fn user_envelope(
        key: &SessionKey,
        conv: &ConversationId,
        seq: u64,
        request: TagmaRequest,
    ) -> Envelope {
        let bytes = serde_json::to_vec(&request).unwrap();
        Envelope {
            conversation_id: conv.clone(),
            sender: Participant::User {
                user_id: UserId::from("u".to_string()),
            },
            sequence_n: seq,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(initiator_encrypt(key, seq, &bytes)),
        }
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
    async fn pump_maps_sse_to_tagma_events() {
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
        let (handle, key, capture, _prompt_rx, _root_id, state) = setup(16).await;
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

        // Drain until the three in-capability events arrive (or time out).
        let mut got = Vec::new();
        for _ in 0..300 {
            got = drain_replies(&capture, &key).await;
            if got.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.stop_pump().await;
        assert_eq!(
            got.len(),
            3,
            "exactly the in-capability events must be emitted"
        );
        assert!(matches!(
            got[0],
            TagmaReply::Event {
                event: TagmaEvent::Busy
            }
        ));
        assert!(matches!(
            got[1],
            TagmaReply::Event {
                event: TagmaEvent::AssistantContent { .. }
            }
        ));
        assert!(matches!(
            got[2],
            TagmaReply::Event {
                event: TagmaEvent::Idle
            }
        ));
    }

    #[test]
    fn map_sse_event_keeps_and_drops_correctly() {
        // Kept variants map one-to-one.
        assert!(matches!(
            ops::map_sse_event(&SseEvent::Busy),
            Some(TagmaEvent::Busy)
        ));
        assert!(matches!(
            ops::map_sse_event(&SseEvent::Interrupted),
            Some(TagmaEvent::Interrupted)
        ));
        assert!(matches!(
            ops::map_sse_event(&SseEvent::MaxRoundsExceeded),
            Some(TagmaEvent::MaxRoundsExceeded)
        ));
        assert!(matches!(
            ops::map_sse_event(&SseEvent::TokenBudgetExceeded {
                consumed: 1,
                budget: 2
            }),
            Some(TagmaEvent::TokenBudgetExceeded {
                consumed: 1,
                budget: 2
            })
        ));
        // Dropped (out-of-capability) variants.
        assert!(
            ops::map_sse_event(&SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into()
            })
            .is_none()
        );
        assert!(
            ops::map_sse_event(&SseEvent::AssistantContentDelta { delta: "d".into() }).is_none()
        );
        assert!(
            ops::map_sse_event(&SseEvent::ApprovalUpdated {
                id: "a".into(),
                status: kallip_common::approval::ApprovalStatus::Pending
            })
            .is_none()
        );
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

    #[tokio::test]
    async fn emit_message_delivers_assistant_content() {
        // The message-delivery path emits an AssistantContent envelope under the
        // active epoch key; the burst limiter allows it.
        let (handle, key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        handle
            .emit_message("hello user".to_string())
            .await
            .expect("emit_message");
        let replies = drain_replies(&capture, &key).await;
        assert!(matches!(
            replies.as_slice(),
            [TagmaReply::Event {
                event: TagmaEvent::AssistantContent { content }
            }] if content == "hello user"
        ));
    }

    #[tokio::test]
    async fn emit_message_enforces_burst_cap() {
        // Within one 10s window the limiter admits at most MESSAGE_BURST_MAX
        // (20) deliveries; the next is rejected with BurstExceeded.
        let (handle, _key, capture, _prompt_rx, _root_id, _state) = setup(1).await;
        let mut denied = false;
        for i in 0..25 {
            if handle.emit_message(format!("message {i}")).await.is_err() {
                denied = true;
                break;
            }
        }
        assert!(denied, "the burst cap must eventually deny a message");
        // The cap is 20, so exactly 20 envelopes were posted before the denial.
        assert_eq!(capture.lock().await.len(), 20);
    }

    /// A zero key for the no-KEX drop test (the ciphertext never decrypts; the
    /// op is dropped at the key-absent check before decryption anyway).
    fn key_zero() -> SessionKey {
        SessionKey::new([0u8; 32])
    }
}
