//! The herald: connects to the agora tunnel, brokers key exchanges, and bridges
//! each decrypted user message to the local `kallip-daemon`, then re-encrypts
//! the agent's reply back through the agora.
//!
//! One in-flight task per agent: the daemon's event stream carries no turn id,
//! so at most one turn per `(team, agent)` is outstanding (held by a per-agent
//! mutex). A terminal event (the set `consume_until_terminal` treats as
//! terminal) or a stream end releases the slot; that function is the
//! authoritative source, so this doc does not duplicate the variant list.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::Ciphertext;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TeamId, TraceId};
use kallip_agora_common::message::{Envelope, Participant, Plaintext, SystemEvent};
use kallip_client::DaemonClient;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::SseEvent;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{info, warn};

use crate::e2e::{self, DeviceKey, SessionKey};

/// A turn's outcome, mapped from the daemon's terminal SseEvents.
enum Terminal {
    Finished(String),
    Error(String),
    Cancelled,
}

#[derive(Clone)]
pub struct Herald {
    inner: Arc<Inner>,
}

/// The herald's shared mutable state. The per-conversation maps (`session_keys`,
/// `conv_agents`, `outbound_seq`, `seen_inbound`) are unbounded within an
/// incarnation (no eviction); bounded in practice by process restart cadence.
struct Inner {
    agora_url: String,
    team_id: TeamId,
    team_token: String,
    http: reqwest::Client,
    daemon: DaemonClient,
    device: DeviceKey,
    /// Per-conversation AEAD key, established at key exchange.
    session_keys: Mutex<HashMap<ConversationId, SessionKey>>,
    /// Per-conversation bound agent (learned at key exchange).
    conv_agents: Mutex<HashMap<ConversationId, AgentId>>,
    /// Per-agent in-flight mutex (the one-in-flight constraint).
    inflight: Mutex<HashMap<AgentId, Arc<Mutex<()>>>>,
    /// Per-conversation outgoing sequence counter.
    outbound_seq: Mutex<HashMap<ConversationId, u64>>,
    /// Highest inbound (app->herald) `sequence_n` seen per conversation. The
    /// AEAD does not itself reject replay, so this window is the receiver-side
    /// guard against re-delivery. In-memory only: lost on herald restart, but a
    /// restart also clears `session_keys` (forcing re-KEX), so a replayed
    /// ciphertext in a stale session is moot.
    seen_inbound: Mutex<HashMap<ConversationId, u64>>,
}

impl Herald {
    pub fn new(
        agora_url: String,
        team_id: TeamId,
        team_token: String,
        daemon: DaemonClient,
        device: DeviceKey,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                agora_url,
                team_id,
                team_token,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .expect("build reqwest client"),
                daemon,
                device,
                session_keys: Mutex::new(HashMap::new()),
                conv_agents: Mutex::new(HashMap::new()),
                inflight: Mutex::new(HashMap::new()),
                outbound_seq: Mutex::new(HashMap::new()),
                seen_inbound: Mutex::new(HashMap::new()),
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
    /// task so a long agent turn does not stall the stream reader).
    async fn connect_and_drain(self) -> Result<()> {
        let url = format!("{}/v1/herald/tunnel", self.inner.agora_url);
        // Reconnect proof: a timestamp + signature over the tunnel transcript,
        // so a stolen team token alone cannot open a tunnel.
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let proof = self
            .inner
            .device
            .sign(&kallip_agora_common::proof::tunnel_transcript(
                self.inner.team_id.as_ref(),
                unix_secs,
            ));
        let proof_b64 = base64::engine::general_purpose::STANDARD.encode(proof);
        let resp = self
            .inner
            .http
            .get(&url)
            .bearer_auth(&self.inner.team_token)
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
                agent_id,
                init,
            } => {
                let (response, key) = match e2e::respond_key_exchange(
                    &self.inner.device,
                    self.inner.team_id.as_ref(),
                    conversation_id.as_ref(),
                    agent_id.as_ref(),
                    &init,
                ) {
                    Ok(x) => x,
                    Err(e) => {
                        warn!(conv = %conversation_id, "key exchange failed: {e:#}");
                        return;
                    }
                };
                self.inner
                    .session_keys
                    .lock()
                    .await
                    .insert(conversation_id.clone(), key);
                self.inner
                    .conv_agents
                    .lock()
                    .await
                    .insert(conversation_id.clone(), agent_id);
                if let Err(e) = self
                    .post_key_exchange_response(&conversation_id, &response)
                    .await
                {
                    warn!(conv = %conversation_id, "post key-exchange response: {e:#}");
                }
            }
            HeraldInbound::Envelope { envelope } => {
                let conv_id = envelope.conversation_id.clone();
                // Recover from a panic so a buggy dispatch never silently loses
                // a user turn: emit a TurnError instead. The one-in-flight guard
                // (acquired inside) releases on unwind, so the slot never wedges.
                let result = AssertUnwindSafe(self.handle_user_envelope(envelope))
                    .catch_unwind()
                    .await;
                if result.is_err() {
                    warn!(conv = %conv_id, "turn dispatch panicked; emitting TurnError");
                    self.emit_turn_error(&conv_id).await;
                }
            }
        }
    }

    async fn handle_user_envelope(&self, envelope: Envelope) {
        let conv_id = envelope.conversation_id.clone();
        let Participant::User { .. } = &envelope.sender else {
            return; // only user->herald envelopes drive the daemon
        };
        // Receiver-side replay window: reject a re-delivered/replayed
        // sequence_n before doing any work. (The AEAD does not itself reject
        // replay; the agora's seq_seen covers within-incarnation re-delivery,
        // this covers the rest.)
        {
            let mut seen = self.inner.seen_inbound.lock().await;
            if let Some(highest) = seen.get(&conv_id)
                && envelope.sequence_n <= *highest
            {
                warn!(conv = %conv_id, seq = envelope.sequence_n, "replayed inbound envelope; dropping");
                return;
            }
            seen.insert(conv_id.clone(), envelope.sequence_n);
        }
        let key = match self.inner.session_keys.lock().await.get(&conv_id).copied() {
            Some(k) => k,
            None => {
                warn!(conv = %conv_id, "envelope before key exchange; dropping");
                return;
            }
        };
        let plain = match e2e::decrypt(&key, envelope.sequence_n, &envelope.ciphertext.0) {
            Some(p) => p,
            None => {
                warn!(conv = %conv_id, "decrypt failed; dropping");
                return;
            }
        };
        let plaintext: Plaintext = match serde_json::from_slice(&plain) {
            Ok(p) => p,
            Err(e) => {
                warn!(conv = %conv_id, "plaintext decode failed: {e}");
                return;
            }
        };
        let Plaintext::Text { text, .. } = plaintext else {
            return; // only text drives a turn in phase 1
        };
        let agent_id = match self.inner.conv_agents.lock().await.get(&conv_id).cloned() {
            Some(a) => a,
            None => {
                warn!(conv = %conv_id, "no bound agent; dropping");
                return;
            }
        };

        // One in-flight task per agent (daemon event stream has no turn id).
        let _guard = self.acquire_inflight(&agent_id).await;
        if let Err(e) = self.inner.daemon.post_message(&agent_id, &text).await {
            warn!(conv = %conv_id, "post_message: {e:#}");
            return;
        }
        let terminal = self.consume_until_terminal(&agent_id).await;
        self.emit_terminal(&conv_id, &agent_id, terminal).await;
    }

    async fn acquire_inflight(&self, agent_id: &AgentId) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.inner.inflight.lock().await;
            map.entry(agent_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }

    /// Consume the daemon event stream until a terminal event (or stream end).
    async fn consume_until_terminal(&self, agent_id: &AgentId) -> Terminal {
        let mut stream = match self.inner.daemon.event_stream(agent_id).await {
            Ok(s) => s,
            Err(e) => return Terminal::Error(format!("event stream: {e:#}")),
        };
        while let Some(item) = stream.next().await {
            match item {
                Ok(SseEvent::Finished { content }) => return Terminal::Finished(content),
                Ok(SseEvent::Error { message }) => return Terminal::Error(message),
                Ok(SseEvent::MaxRoundsExceeded) => {
                    return Terminal::Error("max rounds exceeded".into());
                }
                Ok(SseEvent::TokenBudgetExceeded { .. }) => {
                    return Terminal::Error("token budget exceeded".into());
                }
                Ok(SseEvent::FailoverChainExhausted { .. }) => {
                    return Terminal::Error("failover chain exhausted".into());
                }
                Ok(SseEvent::Cancelled | SseEvent::Interrupted) => return Terminal::Cancelled,
                Ok(_) | Err(_) => continue,
            }
        }
        Terminal::Error("stream ended without a terminal event".into())
    }

    async fn emit_terminal(
        &self,
        conv_id: &ConversationId,
        agent_id: &AgentId,
        terminal: Terminal,
    ) {
        let plaintext = match terminal {
            Terminal::Finished(content) => Plaintext::Text {
                text: content,
                parent: None,
            },
            Terminal::Error(message) => Plaintext::System {
                event: SystemEvent::TurnError { message },
            },
            Terminal::Cancelled => return, // emit nothing for a cancelled turn
        };
        self.emit_plaintext(conv_id, agent_id, plaintext).await;
    }

    /// Best-effort error emitted when a turn dispatch panicked, so the user does
    /// not see a silent hang.
    async fn emit_turn_error(&self, conv_id: &ConversationId) {
        let agent_id = match self.inner.conv_agents.lock().await.get(conv_id).cloned() {
            Some(a) => a,
            None => return,
        };
        self.emit_plaintext(
            conv_id,
            &agent_id,
            Plaintext::System {
                event: SystemEvent::TurnError {
                    message: "internal error: turn panicked".into(),
                },
            },
        )
        .await;
    }

    /// Encrypt `plaintext` for `conv_id` and post the agent envelope, retrying
    /// on 503 (app briefly offline) with a bounded backoff.
    async fn emit_plaintext(
        &self,
        conv_id: &ConversationId,
        agent_id: &AgentId,
        plaintext: Plaintext,
    ) {
        let key = match self.inner.session_keys.lock().await.get(conv_id).copied() {
            Some(k) => k,
            None => {
                warn!(conv = %conv_id, "emit_plaintext with no session key; dropping reply");
                return;
            }
        };
        let seq = {
            let mut counters = self.inner.outbound_seq.lock().await;
            let entry = counters.entry(conv_id.clone()).or_insert(0);
            let s = *entry;
            *entry += 1;
            s
        };
        let json = match serde_json::to_vec(&plaintext) {
            Ok(v) => v,
            Err(_) => return,
        };
        let ciphertext = e2e::encrypt(&key, seq, &json);
        let envelope = Envelope {
            conversation_id: conv_id.clone(),
            sender: Participant::Agent {
                team_id: self.inner.team_id.clone(),
                agent_id: agent_id.clone(),
            },
            sequence_n: seq,
            trace_id: TraceId::random(),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(ciphertext),
        };
        if let Err(e) = self.post_agent_envelope(conv_id, &envelope).await {
            warn!(conv = %conv_id, "post agent envelope: {e:#}");
        }
    }

    /// Post an agent envelope, retrying on 503 (app offline) with a bounded
    /// backoff (500ms, 1s, 2s, 4s, 8s, 16s ~= 31s total). A dropped reply is
    /// recovered by the app's host-history re-pull on reconnect, so the retry
    /// only rides out transient reconnects.
    async fn post_agent_envelope(
        &self,
        conv_id: &ConversationId,
        envelope: &Envelope,
    ) -> Result<()> {
        let url = format!(
            "{}/v1/conversations/{conv_id}/envelopes",
            self.inner.agora_url
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
                .http
                .post(&url)
                .bearer_auth(&self.inner.team_token)
                .json(envelope)
                .send()
                .await
                .context("agora POST failed")?;
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            // Retry only on 503 (peer offline). Other failures are not transient.
            if status.as_u16() != 503 {
                anyhow::bail!("agora POST returned {}", status);
            }
            tokio::time::sleep(wait).await;
        }
        anyhow::bail!("agora POST exhausted retries (app offline)");
    }

    async fn post_key_exchange_response(
        &self,
        conv_id: &ConversationId,
        response: &kallip_agora_common::control::KeyExchangeResponse,
    ) -> Result<()> {
        let url = format!(
            "{}/v1/conversations/{conv_id}/key-exchange/response",
            self.inner.agora_url
        );
        self.post_with_team_auth(&url, response).await
    }

    async fn post_with_team_auth<T: serde::Serialize>(&self, url: &str, body: &T) -> Result<()> {
        let resp = self
            .inner
            .http
            .post(url)
            .bearer_auth(&self.inner.team_token)
            .json(body)
            .send()
            .await
            .context("agora POST failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("agora POST returned {}", resp.status());
        }
        Ok(())
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
