//! The herald: connects to the agora tunnel, brokers key exchanges, and exposes
//! the tagma as a single stateful entity to remote apps.
//!
//! The tagma owns exactly one conversation with its operator, addressed as a
//! whole: the app sends semantic operations ([`TagmaRequest`] - send a message,
//! interrupt), and the herald translates each to a typed daemon call against the
//! tagma's single persistent root agent. Which agent(s) actually do the work is
//! purely herald-internal and invisible to both app and agora (the root agent
//! may spawn its own sub-tree via tools). A long-lived event pump maps the
//! daemon's SSE stream onto the agent-free [`TagmaEvent`] vocabulary.
//!
//! All app<->herald traffic rides inside E2EE envelopes (AEAD); the agora reads
//! only routing metadata. Key exchange establishes the per-conversation session
//! key; a re-KEX cancels and restarts the pump, rotates the key, and resets the
//! sequence window - on the same stable conversation (its id is derived from the
//! tagma, so it survives reconnects and agora restarts).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::event::{FailoverChainExhaustion, TagmaEvent};
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId};
use kallip_agora_common::message::{Envelope, Participant, TagmaReply, TagmaRequest};
use kallip_client::DaemonClient;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ApiError, FailoverChainExhaustion as DaemonFailover, SseEvent};
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::e2e::{self, DeviceKey, SessionKey};

/// Backoff between daemon event-stream reconnect attempts.
const PUMP_RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Trace id stamped on every event-pump envelope (the pump is one logical
/// producer, not correlated to any single op `req_id`).
const PUMP_TRACE: &str = "herald:pump";

#[derive(Clone)]
pub struct Herald {
    inner: Arc<Inner>,
}

/// The herald's shared state. The conversation is 1:1 with the tagma, so the
/// crypto state is single-valued (not keyed by conversation).
struct Inner {
    /// Lesche (data-plane relay) base URL: the herald's tunnel + envelope +
    /// key-exchange POSTs all go here now that the relay is a separate service
    /// from the agora.
    lesche_url: String,
    tagma_id: TagmaId,
    /// The single conversation this tagma owns with its operator:
    /// `ConversationId::for_tagma(&tagma_id)`. Stable across reconnects.
    conversation_id: ConversationId,
    tagma_token: String,
    /// Client for one-shot POSTs (envelopes, key-exchange). Carries a total
    /// timeout, which is correct for a request/reply with a natural end.
    http_post: reqwest::Client,
    /// Client for the long-lived herald tunnel (`GET /v1/herald/tunnel`). Has
    /// NO total timeout: `reqwest`'s `.timeout()` is a whole-response deadline
    /// that also covers the streaming body, so any finite value would kill the
    /// tunnel mid-flight every N seconds. Same reasoning as the daemon
    /// event-pump client in `main.rs` (no-timeout, load-bearing for the stream).
    http_stream: reqwest::Client,
    daemon: DaemonClient,
    device: DeviceKey,
    /// The daemon's single root agent id (the daemon owns/creates it at startup;
    /// the herald binds to it via `get_root_agent` and targets it for all ops).
    root_agent: AgentId,
    /// AEAD session key + both sequence counters, all under one lock so an emit
    /// always reads a key/counter pair from the same epoch (a re-KEX rotates the
    /// key and resets the counters atomically). Without this, a KEX racing an
    /// emit could pair an old key with a reset counter (undecryptable reply).
    crypto: Mutex<CryptoState>,
    /// The running event pump, if any. Restarted on each KEX so a re-KEX can
    /// reset the outbound counter with no in-flight emits under the old key.
    pump: Mutex<Option<PumpHandle>>,
}

/// The per-epoch crypto state, mutated atomically under `Inner::crypto`.
struct CryptoState {
    /// Per-conversation AEAD key, established (and rotated on re-KEX) at key
    /// exchange. `None` before the first successful KEX.
    key: Option<SessionKey>,
    /// Outgoing sequence counter. Reset to 0 on every KEX.
    outbound_seq: u64,
    /// Highest inbound (app->herald) `sequence_n` seen THIS crypto epoch.
    /// `None` = no message has arrived yet in the epoch (also the value a KEX
    /// resets to). `Option` (not `u64`) is load-bearing: the first message of a
    /// fresh epoch legitimately carries `sequence_n = 0`, and a plain `u64`
    /// initialized to 0 would reject it (`0 <= 0`). Cross-epoch replay of an
    /// old-key ciphertext is caught because the KEX rotated `key` (read under
    /// the same lock as this field) before the replay arrives, so AEAD decrypt
    /// fails -- the window only needs to cover within-epoch replay.
    seen_inbound: Option<u64>,
}

/// A running pump task plus the token that stops it.
struct PumpHandle {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Herald {
    pub fn new(
        lesche_url: String,
        tagma_id: TagmaId,
        tagma_token: String,
        daemon: DaemonClient,
        device: DeviceKey,
        root_agent: AgentId,
    ) -> Self {
        let conversation_id = ConversationId::for_tagma(&tagma_id);
        Self {
            inner: Arc::new(Inner {
                lesche_url,
                tagma_id,
                conversation_id,
                tagma_token,
                http_post: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("build POST reqwest client"),
                http_stream: reqwest::Client::builder()
                    .build()
                    .expect("build stream reqwest client"),
                daemon,
                device,
                root_agent,
                crypto: Mutex::new(CryptoState {
                    key: None,
                    outbound_seq: 0,
                    seen_inbound: None,
                }),
                pump: Mutex::new(None),
            }),
        }
    }

    /// Hold the agora tunnel open, reconnecting with a small backoff on any
    /// disconnect or error.
    pub async fn run(self) {
        loop {
            match self.clone().connect_and_drain().await {
                Ok(()) => info!("tunnel stream ended; reconnecting"),
                Err(e) => warn!("tunnel error: {e:#}; reconnecting"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Open the tunnel SSE and dispatch each inbound message (each on its own
    /// task so a long-running op does not stall the stream reader).
    async fn connect_and_drain(self) -> Result<()> {
        let url = format!("{}/v1/herald/tunnel", self.inner.lesche_url);
        // Reconnect proof: a timestamp + signature over the tunnel transcript,
        // so a stolen tagma token alone cannot open a tunnel.
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let proof = self
            .inner
            .device
            .sign(&kallip_agora_common::proof::tunnel_transcript(
                self.inner.tagma_id.as_ref(),
                unix_secs,
            ));
        let proof_b64 = base64::engine::general_purpose::STANDARD.encode(proof);
        let resp = self
            .inner
            .http_stream
            .get(&url)
            .bearer_auth(&self.inner.tagma_token)
            .header("X-Device-Timestamp", unix_secs.to_string())
            .header("X-Device-Proof", proof_b64)
            .send()
            .await
            .context("tunnel GET failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("tunnel GET returned {}", resp.status());
        }
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("tunnel chunk")?;
            buf.push_str(std::str::from_utf8(&chunk).context("non-utf8 SSE chunk")?);
            while let Some(idx) = buf.find("\n\n") {
                let event = buf[..idx].to_string();
                buf.drain(..=idx + 1);
                if let Some(data) = parse_data_payload(&event) {
                    match serde_json::from_str::<HeraldInbound>(&data) {
                        Ok(inbound) => {
                            tokio::spawn(self.clone().dispatch(inbound));
                        }
                        Err(e) => warn!("invalid herald inbound JSON: {e}"),
                    }
                }
            }
        }
        Ok(())
    }

    async fn dispatch(self, inbound: HeraldInbound) {
        match inbound {
            HeraldInbound::KeyExchange {
                conversation_id,
                init,
            } => self.handle_kex(conversation_id, init).await,
            HeraldInbound::Envelope { envelope } => {
                // Outer last-resort: a panic before `req_id` is parsed cannot be
                // attributed to an op, so we can only log. The req_id-aware
                // recovery inside `handle_user_op` covers the common case.
                if AssertUnwindSafe(self.handle_user_op(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    warn!("op dispatch panicked before req_id was known");
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
        // Rotate the key and reset both counters atomically. Concurrent KEX is
        // prevented upstream by the agora's one-in-flight-KEX-per-conversation
        // guard; this single lock additionally makes the rotation atomic against
        // any concurrent `emit`/`handle_user_op`.
        {
            let mut c = self.inner.crypto.lock().await;
            c.key = Some(key);
            c.outbound_seq = 0;
            c.seen_inbound = None;
        }
        self.start_pump().await;
        if let Err(e) = self
            .post_key_exchange_response(&conversation_id, &response)
            .await
        {
            warn!(conv = %conversation_id, "post key-exchange response: {e:#}");
        }
    }

    /// Decrypt an app op envelope, run it against the root agent, and emit the
    /// reply. The daemon call runs under a req_id-aware panic boundary so a bug
    /// never leaves the app hanging: a panic yields an `Error` reply for the
    /// exact `req_id`.
    async fn handle_user_op(&self, envelope: Envelope) {
        let Participant::User { .. } = &envelope.sender else {
            return; // only user->herald envelopes drive ops
        };
        // Receiver-side replay window + key read under one lock, so the
        // seen-inbound update and the decrypt key come from the same epoch.
        let key = {
            let mut c = self.inner.crypto.lock().await;
            if let Some(prev) = c.seen_inbound
                && envelope.sequence_n <= prev
            {
                warn!(
                    seq = envelope.sequence_n,
                    "replayed inbound envelope; dropping"
                );
                return;
            }
            c.seen_inbound = Some(envelope.sequence_n);
            match c.key {
                Some(k) => k,
                None => {
                    warn!("op envelope before key exchange; dropping");
                    return;
                }
            }
        };
        let plain = match e2e::decrypt(&key, envelope.sequence_n, &envelope.ciphertext.0) {
            Some(p) => p,
            None => {
                warn!("op decrypt failed; dropping");
                return;
            }
        };
        let request: TagmaRequest = match serde_json::from_slice(&plain) {
            Ok(p) => p,
            Err(e) => {
                warn!("op decode failed: {e}");
                return;
            }
        };
        let req_id = req_id_of(&request);
        let trace = TraceId::from(format!("op:{req_id}"));

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
                    message: "herald op panicked".to_string(),
                }
            }
        };
        if let Err(e) = self.emit(&trace, reply).await {
            warn!(req_id, "emit reply: {e:#}");
        }
    }

    /// Translate one op into a daemon call against the root agent and produce
    /// the matching reply.
    async fn execute_op(&self, request: TagmaRequest) -> Result<TagmaReply> {
        match request {
            TagmaRequest::SendMessage { req_id, text } => {
                let resp: kallip_common::protocol::MessageResponse = self
                    .inner
                    .daemon
                    .post_message(&self.inner.root_agent, &text)
                    .await?;
                Ok(TagmaReply::MessageAccepted {
                    req_id,
                    queue_depth: resp.queue_depth,
                    warning: resp.warning,
                })
            }
            TagmaRequest::Interrupt { req_id } => {
                self.inner
                    .daemon
                    .interrupt_agent(&self.inner.root_agent)
                    .await?;
                Ok(TagmaReply::Interrupted { req_id })
            }
        }
    }

    /// Ensure the pump task is running. Idempotent: a no-op if one is already
    /// live. The pump reads the current session key per-emit, so a later
    /// re-KEX's rotated key is picked up by the *next* pump incarnation.
    async fn start_pump(&self) {
        let mut slot = self.inner.pump.lock().await;
        if slot.is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(self.clone().run_pump(cancel.clone()));
        *slot = Some(PumpHandle { task, cancel });
    }

    /// Stop and await the pump if it is running, clearing the slot so a later
    /// `start_pump` can install a fresh one.
    async fn stop_pump(&self) {
        let handle = { self.inner.pump.lock().await.take() };
        if let Some(handle) = handle {
            handle.cancel.cancel();
            // Await so any in-flight emit completes (or errors) before we touch
            // the crypto state - this is what makes the re-KEX reset race-free.
            let _ = handle.task.await;
        }
    }

    /// Map the daemon's SSE stream onto the tagma-facing event vocabulary and
    /// emit each event. Self-reconnecting: the daemon SSE is independent of the
    /// agora tunnel, so this loop covers daemon restarts; the agora tunnel has
    /// its own reconnect loop in `run`. Stops cleanly when `cancel` fires (on
    /// re-KEX or shutdown).
    async fn run_pump(self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            match self.inner.daemon.event_stream(&self.inner.root_agent).await {
                Ok(mut stream) => loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        event = stream.next() => match event {
                            Some(Ok(sse)) => {
                                if let Some(tagma_ev) = map_sse_event(&sse) {
                                    let trace = TraceId::from(PUMP_TRACE.to_string());
                                    if let Err(e) = self
                                        .emit(&trace, TagmaReply::Event { event: tagma_ev })
                                        .await
                                    {
                                        warn!("emit pump event: {e:#}");
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                warn!("daemon event stream item error: {e:#}");
                                break;
                            }
                            None => {
                                info!("daemon event stream ended; reconnecting");
                                break;
                            }
                        },
                    }
                },
                Err(e) => warn!("open daemon event stream: {e:#}"),
            }
            if cancel.is_cancelled() {
                return;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(PUMP_RECONNECT_BACKOFF) => {}
            }
        }
    }

    /// Encrypt `reply` for the conversation and post the agent envelope, retrying
    /// on 503 (app briefly offline) with a bounded backoff. Returns `Err` when
    /// delivery is exhausted so a streaming producer could stop (the pump logs
    /// and carries on; app recovery is via host-history re-pull on reconnect).
    async fn emit(&self, trace: &TraceId, reply: TagmaReply) -> Result<()> {
        // Read the key and reserve the seq under one lock so they are always
        // from the same epoch (a concurrent re-KEX cannot split them).
        let (key, seq) = {
            let mut c = self.inner.crypto.lock().await;
            let key = match c.key {
                Some(k) => k,
                None => {
                    // No session key yet: the app is not connected. Drop
                    // silently; the pump will re-emit live events once a KEX
                    // completes.
                    return Ok(());
                }
            };
            let seq = c.outbound_seq;
            c.outbound_seq += 1;
            (key, seq)
        };
        let json = serde_json::to_vec(&reply).context("encode tagma reply")?;
        let ciphertext = e2e::encrypt(&key, seq, &json);
        let envelope = Envelope {
            conversation_id: self.inner.conversation_id.clone(),
            sender: Participant::Agent {
                tagma_id: self.inner.tagma_id.clone(),
            },
            sequence_n: seq,
            trace_id: trace.clone(),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(ciphertext),
        };
        match self.post_agent_envelope(&envelope).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Deliberately do NOT roll back `outbound_seq`. The seq was
                // already mixed into the AEAD nonce, and a POST can fail after
                // the relay has accepted and forwarded the envelope (e.g. a
                // response read error mid-body), so reusing the seq would risk
                // a nonce reuse under the same epoch key with a different
                // plaintext. Burning a gap is safe instead: the app applies
                // envelopes by decryption, not by sequence validation, and
                // recovers any lost reply via host-history re-pull on reconnect.
                Err(e)
            }
        }
    }

    /// Post an agent envelope, retrying on 503 (app offline) with a bounded
    /// backoff (500ms, 1s, 2s, 4s, 8s, 16s ~= 31s total). A dropped reply is
    /// recovered by the app's host-history re-pull on reconnect, so the retry
    /// only rides out transient reconnects.
    async fn post_agent_envelope(&self, envelope: &Envelope) -> Result<()> {
        let url = format!(
            "{}/v1/conversations/{}/envelopes",
            self.inner.lesche_url, self.inner.conversation_id
        );
        const BACKOFF: [Duration; 6] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ];
        for wait in BACKOFF {
            let resp = self
                .inner
                .http_post
                .post(&url)
                .bearer_auth(&self.inner.tagma_token)
                .json(envelope)
                .send()
                .await
                .context("lesche POST failed")?;
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            // Retry only on 503 (peer offline). Other failures are not transient.
            if status.as_u16() != 503 {
                anyhow::bail!("lesche POST returned {}", status);
            }
            tokio::time::sleep(wait).await;
        }
        anyhow::bail!("lesche POST exhausted retries (app offline)")
    }

    async fn post_key_exchange_response(
        &self,
        conv_id: &ConversationId,
        response: &kallip_agora_common::control::KeyExchangeResponse,
    ) -> Result<()> {
        let url = format!(
            "{}/v1/conversations/{conv_id}/key-exchange/response",
            self.inner.lesche_url
        );
        self.post_with_tagma_auth(&url, response).await
    }

    async fn post_with_tagma_auth<T: serde::Serialize>(&self, url: &str, body: &T) -> Result<()> {
        let resp = self
            .inner
            .http_post
            .post(url)
            .bearer_auth(&self.inner.tagma_token)
            .json(body)
            .send()
            .await
            .context("lesche POST failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("lesche POST returned {}", resp.status());
        }
        Ok(())
    }
}

/// The `req_id` of a request, extracted before any fallible daemon call so a
/// panic during the call can still be attributed to the right op.
fn req_id_of(request: &TagmaRequest) -> u64 {
    match request {
        TagmaRequest::SendMessage { req_id, .. } | TagmaRequest::Interrupt { req_id } => *req_id,
    }
}

/// Map a daemon error to an op `Error` reply, preserving the daemon's HTTP
/// status when the error carries one (otherwise 502 bad gateway).
fn op_err_reply(req_id: u64, e: &anyhow::Error) -> TagmaReply {
    let status = e
        .downcast_ref::<ApiError>()
        .map(|a| a.status)
        .unwrap_or(502);
    TagmaReply::Error {
        req_id,
        status,
        message: format!("{e:#}"),
    }
}

/// Map a daemon `SseEvent` to the agent-free tagma-facing vocabulary, dropping
/// variants outside the app's capability set (streaming deltas, tool events,
/// retry/failover telemetry, approval updates) with a `debug!` so a new daemon
/// event never vanishes silently.
fn map_sse_event(sse: &SseEvent) -> Option<TagmaEvent> {
    Some(match sse {
        SseEvent::AssistantContent { content } => TagmaEvent::AssistantContent {
            content: content.clone(),
        },
        SseEvent::Finished { content } => TagmaEvent::Finished {
            content: content.clone(),
        },
        SseEvent::Busy => TagmaEvent::Busy,
        SseEvent::Status { message } => TagmaEvent::Status {
            message: message.clone(),
        },
        SseEvent::Error { message } => TagmaEvent::Error {
            message: message.clone(),
        },
        SseEvent::Interrupted => TagmaEvent::Interrupted,
        SseEvent::Cancelled => TagmaEvent::Cancelled,
        SseEvent::TokenBudgetExceeded { consumed, budget } => TagmaEvent::TokenBudgetExceeded {
            consumed: *consumed,
            budget: *budget,
        },
        SseEvent::MaxRoundsExceeded => TagmaEvent::MaxRoundsExceeded,
        SseEvent::FailoverChainExhausted { reason, detail } => TagmaEvent::FailoverChainExhausted {
            reason: map_failover_exhaustion(*reason),
            detail: detail.clone(),
        },
        other => {
            debug!(target: "herald.sse_drop", event = ?other, "dropping out-of-capability daemon event");
            return None;
        }
    })
}

fn map_failover_exhaustion(reason: DaemonFailover) -> FailoverChainExhaustion {
    match reason {
        DaemonFailover::NoFailoverConfigured => FailoverChainExhaustion::NoFailoverConfigured,
        DaemonFailover::AllBackupsExhausted => FailoverChainExhaustion::AllBackupsExhausted,
        DaemonFailover::AllCandidatesUnbuildable => {
            FailoverChainExhaustion::AllCandidatesUnbuildable
        }
        DaemonFailover::AllCandidatesInfeasible => FailoverChainExhaustion::AllCandidatesInfeasible,
    }
}

/// Extract the concatenated `data:` payload from one SSE event block.
fn parse_data_payload(event: &str) -> Option<String> {
    let mut data = String::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start_matches(' '));
        }
    }
    (!data.is_empty()).then_some(data)
}

#[cfg(test)]
mod op_tests {
    //! Operation-level tests: a mock daemon (axum) + a mock agora that captures
    //! posted envelopes, driven by the real herald. The "app" side is simulated
    //! inline (dir-0 encrypt of the request, dir-1 decrypt of the replies). This
    //! proves the semantic channel - encrypt -> herald op -> daemon call ->
    //! encrypt reply -> decrypt - without the real agora or any TS.

    use super::*;
    use crate::e2e::SessionKey;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::{Router, routing::get, routing::post};
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
    use kallip_agora_common::bytes::Ciphertext;
    use kallip_agora_common::control::KeyExchangeInit;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId, UserId};
    use kallip_agora_common::message::{Envelope, Participant, TagmaReply, TagmaRequest};
    use kallip_common::protocol::SseEvent;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Captured outbound envelopes, in arrival order.
    type Capture = Arc<Mutex<Vec<Envelope>>>;
    /// Recorded message texts delivered to the mock daemon.
    type Messages = Arc<Mutex<Vec<String>>>;
    /// Recorded interrupt calls.
    type Interrupts = Arc<Mutex<u64>>;

    const DIR_APP_TO_HERALD: u32 = 0;
    const DIR_HERALD_TO_APP: u32 = 1;

    fn nonce(dir: u32, seq: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0..4].copy_from_slice(&dir.to_be_bytes());
        n[4..12].copy_from_slice(&seq.to_be_bytes());
        n
    }

    fn app_encrypt(key: &SessionKey, seq: u64, pt: &[u8]) -> Vec<u8> {
        let aead = ChaCha20Poly1305::new(key.into());
        aead.encrypt(&Nonce::from(nonce(DIR_APP_TO_HERALD, seq)), pt)
            .unwrap()
    }

    fn app_decrypt(key: &SessionKey, seq: u64, ct: &[u8]) -> Option<Vec<u8>> {
        let aead = ChaCha20Poly1305::new(key.into());
        aead.decrypt(&Nonce::from(nonce(DIR_HERALD_TO_APP, seq)), ct)
            .ok()
    }

    #[derive(Clone)]
    struct DaemonState {
        messages: Messages,
        interrupts: Interrupts,
        /// Events the `/events` stream drains, in order, then ends.
        events: Arc<Mutex<std::collections::VecDeque<SseEvent>>>,
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

    async fn spawn_daemon(state: DaemonState) -> String {
        let app = Router::new()
            .route("/agents/{id}/message", post(message_handler))
            .route("/agents/{id}/interrupt", post(interrupt_handler))
            .route("/agents/{id}/events", get(events_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn message_handler(
        State(s): State<DaemonState>,
        axum::Json(req): axum::Json<kallip_common::protocol::MessageRequest>,
    ) -> axum::Json<kallip_common::protocol::MessageResponse> {
        s.messages.lock().await.push(req.text);
        axum::Json(kallip_common::protocol::MessageResponse {
            queue_depth: 0,
            warning: None,
        })
    }

    async fn interrupt_handler(State(s): State<DaemonState>) -> StatusCode {
        let mut n = s.interrupts.lock().await;
        *n += 1;
        StatusCode::ACCEPTED
    }

    /// Stream the queued `SseEvent`s as `data: <json>` SSE frames, then end.
    async fn events_handler(State(s): State<DaemonState>) -> Response {
        let mut frames: Vec<Result<Vec<u8>, std::io::Error>> = Vec::new();
        let mut queue = s.events.lock().await;
        while let Some(ev) = queue.pop_front() {
            let json = serde_json::to_string(&ev).unwrap();
            frames.push(Ok(format!("data: {json}\n\n").into_bytes()));
        }
        let body = axum::body::Body::from_stream(futures_util::stream::iter(frames));
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap()
    }

    /// Build a herald wired to fresh mock agora + daemon, with a pre-shared
    /// session key installed (KEX is covered by e2e tests). Returns the capture
    /// handle and the message/interrupt recorders.
    async fn setup(events: Vec<SseEvent>) -> (Herald, SessionKey, Capture, Messages, Interrupts) {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let messages: Messages = Arc::new(Mutex::new(Vec::new()));
        let interrupts: Interrupts = Arc::new(Mutex::new(0));
        let lesche_url = spawn_lesche(capture.clone()).await;
        let daemon_state = DaemonState {
            messages: messages.clone(),
            interrupts: interrupts.clone(),
            events: Arc::new(Mutex::new(events.into())),
        };
        let daemon_url = spawn_daemon(daemon_state).await;
        let daemon = DaemonClient::builder(&daemon_url)
            .auth_token("test")
            .build()
            .unwrap();
        let device = DeviceKey::generate();
        let tagma = TagmaId::from("tagma".to_string());
        let herald = Herald::new(
            lesche_url,
            tagma,
            "tok".to_string(),
            daemon,
            device,
            AgentId::from("root".to_string()),
        );
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("getrandom");
        herald.inner.crypto.lock().await.key = Some(key);
        (herald, key, capture, messages, interrupts)
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
            ciphertext: Ciphertext(app_encrypt(key, seq, &bytes)),
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
                let plain = app_decrypt(key, env.sequence_n, &env.ciphertext.0).unwrap();
                serde_json::from_slice::<TagmaReply>(&plain).unwrap()
            })
            .collect()
    }

    fn conv_of(herald: &Herald) -> ConversationId {
        ConversationId::for_tagma(&herald.inner.tagma_id)
    }

    #[tokio::test]
    async fn send_message_round_trips() {
        let (herald, key, capture, messages, _interrupts) = setup(Vec::new()).await;
        let conv = conv_of(&herald);
        herald
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
        // The daemon received the text.
        assert_eq!(messages.lock().await.as_slice(), &["hello"]);
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
        let (herald, key, capture, _messages, interrupts) = setup(Vec::new()).await;
        let conv = conv_of(&herald);
        herald
            .handle_user_op(user_envelope(
                &key,
                &conv,
                1,
                TagmaRequest::Interrupt { req_id: 7 },
            ))
            .await;
        assert_eq!(*interrupts.lock().await, 1);
        let replies = drain_replies(&capture, &key).await;
        assert!(matches!(
            replies.as_slice(),
            [TagmaReply::Interrupted { req_id: 7 }]
        ));
    }

    #[tokio::test]
    async fn op_before_key_exchange_is_dropped() {
        // A herald with no session key must drop the op silently (no daemon call,
        // no reply).
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let lesche_url = spawn_lesche(capture.clone()).await;
        let daemon_state = DaemonState {
            messages: Arc::new(Mutex::new(Vec::new())),
            interrupts: Arc::new(Mutex::new(0)),
            events: Arc::new(Mutex::new(Vec::new().into())),
        };
        let daemon_url = spawn_daemon(daemon_state).await;
        let daemon = DaemonClient::builder(&daemon_url)
            .auth_token("test")
            .build()
            .unwrap();
        let herald = Herald::new(
            lesche_url,
            TagmaId::from("tagma".to_string()),
            "tok".to_string(),
            daemon,
            DeviceKey::generate(),
            AgentId::from("root".to_string()),
        );
        let key = [0u8; 32];
        let conv = conv_of(&herald);
        herald
            .handle_user_op(user_envelope(
                &key,
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
        let (herald, key, capture, _messages, _interrupts) = setup(Vec::new()).await;
        let conv = conv_of(&herald);
        herald
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
        let (herald, key, capture, _messages, _interrupts) = setup(Vec::new()).await;
        let conv = conv_of(&herald);
        let env = user_envelope(
            &key,
            &conv,
            5,
            TagmaRequest::SendMessage {
                req_id: 1,
                text: "first".into(),
            },
        );
        herald.handle_user_op(env.clone()).await;
        // A replay of the same sequence number is dropped without a second reply.
        herald.handle_user_op(env).await;
        let replies = drain_replies(&capture, &key).await;
        assert_eq!(
            replies.len(),
            1,
            "replayed seq must not produce a second reply"
        );
    }

    #[tokio::test]
    async fn pump_maps_sse_to_tagma_events() {
        let events = vec![
            SseEvent::Busy,
            SseEvent::AssistantContent {
                content: "hi".into(),
            },
            SseEvent::Finished {
                content: "hi".into(),
            },
            SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into(),
            }, // dropped (out of capability)
        ];
        let (herald, key, capture, _messages, _interrupts) = setup(events).await;
        herald.start_pump().await;
        // Drain until the three in-capability events arrive (or time out).
        let mut got = Vec::new();
        for _ in 0..200 {
            got = drain_replies(&capture, &key).await;
            if got.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        herald.stop_pump().await;
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
                event: TagmaEvent::Finished { .. }
            }
        ));
    }

    #[test]
    fn map_sse_event_keeps_and_drops_correctly() {
        // Kept variants map one-to-one.
        assert!(matches!(
            map_sse_event(&SseEvent::Busy),
            Some(TagmaEvent::Busy)
        ));
        assert!(matches!(
            map_sse_event(&SseEvent::Interrupted),
            Some(TagmaEvent::Interrupted)
        ));
        assert!(matches!(
            map_sse_event(&SseEvent::MaxRoundsExceeded),
            Some(TagmaEvent::MaxRoundsExceeded)
        ));
        assert!(matches!(
            map_sse_event(&SseEvent::TokenBudgetExceeded {
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
            map_sse_event(&SseEvent::ToolCall {
                name: "x".into(),
                args: "{}".into()
            })
            .is_none()
        );
        assert!(map_sse_event(&SseEvent::AssistantContentDelta { delta: "d".into() }).is_none());
        assert!(
            map_sse_event(&SseEvent::ApprovalUpdated {
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
        let (herald, _key, _capture, _messages, _interrupts) = setup(Vec::new()).await;
        {
            let mut c = herald.inner.crypto.lock().await;
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
        herald.handle_kex(conv_of(&herald), init).await;

        let c = herald.inner.crypto.lock().await;
        assert!(c.key.is_some(), "KEX must install a session key");
        assert_eq!(c.outbound_seq, 0, "KEX must reset the outbound counter");
        assert_eq!(c.seen_inbound, None, "KEX must reset the inbound window");
        drop(c);
        assert!(
            herald.inner.pump.lock().await.is_some(),
            "KEX must start the pump"
        );
        herald.stop_pump().await;
    }
}
