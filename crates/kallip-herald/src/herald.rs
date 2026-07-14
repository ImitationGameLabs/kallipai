//! The herald: connects to the agora tunnel, brokers key exchanges, and bridges
//! the local daemon's HTTP API to remote apps over an E2EE tunnel.
//!
//! The tunnel is a transparent, daemon-agnostic HTTP reverse proxy. The app
//! sends a [`TunnelFrame::Request`] (method/path/headers/body); the herald
//! performs that request against its local daemon as the operator and streams
//! the response back as `ResponseHead` + `ResponseBody`* + `ResponseEnd` frames.
//! The relay never inspects the bytes, and the herald has no knowledge of what
//! any daemon route means — it forwards any path. (Authz is a separate, deferred
//! concern.)
//!
//! A long-lived response (the daemon's `GET /agents/{id}/events` SSE broadcast)
//! simply never receives a `ResponseEnd` until the daemon stream closes; that is
//! the intended steady state, mirroring the direct client's one long-lived
//! events subscription.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::{FutureExt, StreamExt};
use kallip_agora_common::bytes::{B64, Ciphertext};
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TeamId, TraceId};
use kallip_agora_common::message::{Envelope, Participant, TunnelFrame};
use kallip_client::DaemonClient;
use kallip_common::agentid::AgentId;
use std::panic::AssertUnwindSafe;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::e2e::{self, DeviceKey, SessionKey};

/// Maximum raw bytes carried in one `ResponseBody` frame. Keeps each encrypted
/// envelope comfortably under the agora's request-body limit after base64
/// expansion (~4/3x) and JSON framing.
const CHUNK_CAP: usize = 32 * 1024;

/// Hop-by-hop, transport-framing, and auth headers the app must not control.
/// The herald drops any client-supplied entry whose lowercased name is in this
/// set; the operator `Authorization` is set by the daemon client, and length /
/// connection semantics are owned by reqwest.
const STRIPPED_HEADERS: &[&str] = &[
    "authorization",
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
];

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
    /// Client for agora control-plane POSTs (key-exchange, envelopes). Has a
    /// total timeout, which is correct for one-shot POSTs.
    http: reqwest::Client,
    daemon: DaemonClient,
    device: DeviceKey,
    /// Per-conversation AEAD key, established at key exchange.
    session_keys: Mutex<HashMap<ConversationId, SessionKey>>,
    /// Per-conversation bound agent (learned at key exchange). The conversation
    /// is attributed to this agent on the wire; the tunnel does not key state
    /// per agent because it is `req_id`-multiplexed, not turn-scoped.
    conv_agents: Mutex<HashMap<ConversationId, AgentId>>,
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
    /// task so a long-running tunnel proxy does not stall the stream reader).
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
                // Outer last-resort: a panic before `req_id` is parsed cannot be
                // attributed to a tunnel request, so we can only log. The
                // req_id-aware recovery inside `handle_user_envelope` covers the
                // common case (a panic during the proxy itself).
                if AssertUnwindSafe(self.handle_user_envelope(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    warn!(conv = %conv_id, "envelope dispatch panicked before req_id was known");
                }
            }
        }
    }

    async fn handle_user_envelope(&self, envelope: Envelope) {
        let conv_id = envelope.conversation_id.clone();
        let Participant::User { .. } = &envelope.sender else {
            return; // only user->herald envelopes drive the tunnel
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
        let frame: TunnelFrame = match serde_json::from_slice(&plain) {
            Ok(p) => p,
            Err(e) => {
                warn!(conv = %conv_id, "frame decode failed: {e}");
                return;
            }
        };
        let TunnelFrame::Request { req_id, method, path, headers, body } = frame
        else {
            // Only inbound requests drive the tunnel; response frames are
            // herald->app only and any inbound copy is ignored.
            return;
        };
        let agent_id = match self.inner.conv_agents.lock().await.get(&conv_id).cloned() {
            Some(a) => a,
            None => {
                warn!(conv = %conv_id, "no bound agent; dropping");
                return;
            }
        };

        // req_id-aware panic boundary: the proxy runs under catch_unwind so a
        // bug never leaves the app hanging on `req_id`. A panic yields a 502 +
        // End for exactly this request.
        let req = ProxyReq {
            method,
            path,
            headers,
            body: body.0,
        };
        let result = AssertUnwindSafe(self.proxy_http(&conv_id, &agent_id, req_id, req))
            .catch_unwind()
            .await;
        if result.is_err() {
            warn!(conv = %conv_id, req_id, "tunnel proxy panicked; emitting 502");
            let trace = trace_for(req_id);
            let _ = self.emit(&conv_id, &agent_id, &trace, tunnel_head(req_id, 502, vec![])).await;
            let _ = self.emit(&conv_id, &agent_id, &trace, tunnel_end(req_id)).await;
        }
    }

    /// Proxy one HTTP request to the daemon and stream the response back as
    /// tunnel frames. Returns `Err` only when delivery to the app fails for
    /// good (so the caller can stop feeding a producer whose consumer is gone).
    async fn proxy_http(
        &self,
        conv_id: &ConversationId,
        agent_id: &AgentId,
        req_id: u64,
        req: ProxyReq,
    ) -> Result<()> {
        let trace = trace_for(req_id);
        let sanitized = sanitize_headers(req.headers);

        let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
            Ok(m) => m,
            Err(_) => {
                self.emit_status_and_end(conv_id, agent_id, req_id, &trace, 400)
                    .await;
                return Ok(());
            }
        };
        if !is_safe_path(&req.path) {
            self.emit_status_and_end(conv_id, agent_id, req_id, &trace, 400)
                .await;
            return Ok(());
        }

        let resp = match self
            .inner
            .daemon
            .proxy_request(method, &req.path, &sanitized, Some(&req.body))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(conv = %conv_id, req_id, "daemon proxy request: {e:#}");
                self.emit_status_and_end(conv_id, agent_id, req_id, &trace, 502)
                    .await;
                return Ok(());
            }
        };

        let status = resp.status().as_u16();
        let resp_headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        self.emit(conv_id, agent_id, &trace, tunnel_head(req_id, status, resp_headers))
            .await?;

        // Stream the body byte-agnostically; the herald does not parse SSE.
        // Each reqwest chunk is split to stay under CHUNK_CAP per envelope.
        let mut stream = resp.bytes_stream();
        let mut stream_error: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    for slice in bytes.chunks(CHUNK_CAP) {
                        self.emit(
                            conv_id,
                            agent_id,
                            &trace,
                            TunnelFrame::ResponseBody {
                                req_id,
                                chunk: B64(slice.to_vec()),
                            },
                        )
                        .await?; // delivery exhausted: stop feeding the producer
                    }
                }
                Err(e) => {
                    // The daemon stream broke mid-body. Signal it on the End so
                    // the app fails the response instead of silently truncating.
                    warn!(conv = %conv_id, req_id, "daemon body stream error: {e:#}");
                    stream_error = Some(format!("{e:#}"));
                    break;
                }
            }
        }

        self.emit(
            conv_id,
            agent_id,
            &trace,
            TunnelFrame::ResponseEnd { req_id, error: stream_error },
        )
        .await
    }

    /// Emit a single `ResponseHead{status}` + `ResponseEnd` for a request that
    /// cannot be proxied (bad request / unreachable daemon). Best-effort.
    async fn emit_status_and_end(
        &self,
        conv_id: &ConversationId,
        agent_id: &AgentId,
        req_id: u64,
        trace: &TraceId,
        status: u16,
    ) {
        let _ = self
            .emit(conv_id, agent_id, trace, tunnel_head(req_id, status, vec![]))
            .await;
        let _ = self.emit(conv_id, agent_id, trace, tunnel_end(req_id)).await;
    }

    /// Encrypt `plaintext` for `conv_id` and post the agent envelope, retrying
    /// on 503 (app briefly offline) with a bounded backoff. Returns `Err` when
    /// delivery is exhausted so a streaming producer can stop.
    async fn emit(
        &self,
        conv_id: &ConversationId,
        agent_id: &AgentId,
        trace: &TraceId,
        frame: TunnelFrame,
    ) -> Result<()> {
        let key = match self.inner.session_keys.lock().await.get(conv_id).copied() {
            Some(k) => k,
            None => {
                warn!(conv = %conv_id, "emit with no session key; dropping reply");
                return Ok(());
            }
        };
        let seq = {
            let mut counters = self.inner.outbound_seq.lock().await;
            let entry = counters.entry(conv_id.clone()).or_insert(0);
            let s = *entry;
            *entry += 1;
            s
        };
        let json = serde_json::to_vec(&frame).context("encode tunnel frame")?;
        let ciphertext = e2e::encrypt(&key, seq, &json);
        let envelope = Envelope {
            conversation_id: conv_id.clone(),
            sender: Participant::Agent {
                team_id: self.inner.team_id.clone(),
                agent_id: agent_id.clone(),
            },
            sequence_n: seq,
            trace_id: trace.clone(),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(ciphertext),
        };
        match self.post_agent_envelope(conv_id, &envelope).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Roll back the reserved seq so a frame that never landed does
                // not burn a gap into the delivered stream — otherwise the app's
                // gap detection would kill the whole conversation's stream for
                // one lost chunk. Only roll back if no concurrent emit has since
                // advanced the counter. The agora rolls back its own seq on the
                // same failure, so reuse is safe.
                let mut counters = self.inner.outbound_seq.lock().await;
                if let Some(entry) = counters.get_mut(conv_id)
                    && *entry == seq + 1
                {
                    *entry = seq;
                }
                Err(e)
            }
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
        anyhow::bail!("agora POST exhausted retries (app offline)")
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

/// A tunneled HTTP request's wire fields (everything but `req_id`, which the
/// proxy tracks separately for framing/recovery). Grouped so `proxy_http` stays
/// under the argument-count limit and reads as one logical unit.
struct ProxyReq {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Build a `ResponseHead` tunnel frame.
fn tunnel_head(req_id: u64, status: u16, headers: Vec<(String, String)>) -> TunnelFrame {
    TunnelFrame::ResponseHead {
        req_id,
        status,
        headers,
    }
}

/// Build a clean `ResponseEnd` tunnel frame (no error).
fn tunnel_end(req_id: u64) -> TunnelFrame {
    TunnelFrame::ResponseEnd { req_id, error: None }
}

/// A trace id shared by every frame of one tunnel exchange, so a streamed
/// response's envelopes are correlated in logs.
fn trace_for(req_id: u64) -> TraceId {
    TraceId::from(format!("tunnel:{req_id}"))
}

/// Drop headers the client must not control (auth, hop-by-hop, transport
/// framing). The operator `Authorization` is applied by the daemon client; the
/// rest is owned by reqwest.
fn sanitize_headers(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter(|(name, _)| !STRIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
        .collect()
}

/// A daemon path must be a bare path (start with `/`, no scheme/authority) so a
/// tunneled request cannot be redirected to another host.
fn is_safe_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains("://")
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
mod tunnel_tests {
    //! End-to-end tunnel test: a mock daemon (axum) + a mock agora that captures
    //! posted envelopes, driven by the real herald. The "app" side is simulated
    //! inline (AEAD dir-0 encrypt of the request, dir-1 decrypt of the replies).
    //! This proves the channel — encrypt -> herald proxy -> decrypt -> reassemble
    //! — without the real agora or any TS.

    use super::*;
    use crate::e2e::SessionKey;
    use axum::extract::State;
    use axum::http::header::CONTENT_TYPE;
    use axum::http::StatusCode;
    use axum::response::Response;
    use axum::{Router, routing::get};
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
    use kallip_agora_common::bytes::{B64, Ciphertext};
    use kallip_agora_common::ids::{ConversationId, TeamId, TraceId, UserId};
    use kallip_agora_common::message::{Envelope, Participant, TunnelFrame};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Captured outbound envelopes, in arrival order.
    type Capture = Arc<Mutex<Vec<Envelope>>>;

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
        aead.encrypt(&Nonce::from(nonce(DIR_APP_TO_HERALD, seq)), pt).unwrap()
    }

    fn app_decrypt(key: &SessionKey, seq: u64, ct: &[u8]) -> Option<Vec<u8>> {
        let aead = ChaCha20Poly1305::new(key.into());
        aead.decrypt(&Nonce::from(nonce(DIR_HERALD_TO_APP, seq)), ct).ok()
    }

    async fn spawn_agora(capture: Capture) -> String {
        let app = Router::new()
            .route(
                "/v1/conversations/{conv}/envelopes",
                axum::routing::post(capture_handler),
            )
            .with_state(capture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    async fn capture_handler(State(c): State<Capture>, env: axum::Json<Envelope>) -> &'static str {
        c.lock().await.push(env.0);
        "ok"
    }

    async fn spawn_daemon() -> String {
        let app = Router::new()
            .route(
                "/oneshot",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "application/json")],
                        r#"{"ok":true}"#,
                    )
                }),
            )
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route(
                "/big",
                get(|| async { ([(CONTENT_TYPE, "text/plain")], "x".repeat(100_000)) }),
            )
            .route("/stream", get(stream_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// A response body delivered as separately-flushed chunks, so reqwest's
    /// `bytes_stream` yields multiple items the tunnel must reassemble verbatim.
    async fn stream_handler() -> Response {
        let parts: Vec<Result<Vec<u8>, std::io::Error>> = vec![
            Ok(b"chunk-A-".to_vec()),
            Ok(b"chunk-B-".to_vec()),
            Ok(b"chunk-C-".to_vec()),
        ];
        let body = axum::body::Body::from_stream(futures_util::stream::iter(parts));
        Response::builder()
            .header(CONTENT_TYPE, "text/plain")
            .body(body)
            .unwrap()
    }

    /// Build a herald wired to fresh mock agora + daemon, with a pre-shared
    /// session key installed (KEX is covered by the e2e tests). Returns the
    /// capture handle so the test can read the posted reply envelopes.
    async fn setup() -> (Herald, SessionKey, ConversationId, Capture) {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let agora_url = spawn_agora(capture.clone()).await;
        let daemon_url = spawn_daemon().await;
        let daemon = DaemonClient::builder(&daemon_url).build().unwrap();
        let device = DeviceKey::generate();
        let herald = Herald::new(
            agora_url,
            TeamId::from("team".to_string()),
            "tok".to_string(),
            daemon,
            device,
        );
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("getrandom");
        let conv = ConversationId::from("conv".to_string());
        herald
            .inner
            .session_keys
            .lock()
            .await
            .insert(conv.clone(), key);
        herald
            .inner
            .conv_agents
            .lock()
            .await
            .insert(conv.clone(), AgentId::from("agent".to_string()));
        (herald, key, conv, capture)
    }

    /// Encrypt + send one tunneled request through the herald (does not drain
    /// the capture, so concurrent requests can be sent before a single drain).
    async fn send_request(
        herald: &Herald,
        key: &SessionKey,
        conv: &ConversationId,
        seq: u64,
        req_id: u64,
        method: &str,
        path: &str,
    ) {
        let frame = TunnelFrame::Request {
            req_id,
            method: method.to_string(),
            path: path.to_string(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: B64(Vec::new()),
        };
        let bytes = serde_json::to_vec(&frame).unwrap();
        let envelope = Envelope {
            conversation_id: conv.clone(),
            sender: Participant::User {
                user_id: UserId::from("u".to_string()),
            },
            sequence_n: seq,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(app_encrypt(key, seq, &bytes)),
        };
        herald.handle_user_envelope(envelope).await;
    }

    /// Drain the captured outbound envelopes and decrypt them into tunnel frames.
    async fn drain_frames(capture: &Capture, key: &SessionKey) -> Vec<TunnelFrame> {
        capture
            .lock()
            .await
            .clone()
            .into_iter()
            .map(|env| {
                let plain = app_decrypt(key, env.sequence_n, &env.ciphertext.0).unwrap();
                serde_json::from_slice::<TunnelFrame>(&plain).unwrap()
            })
            .collect()
    }

    /// Reduce the reply frames for one `req_id` to its status, content-type,
    /// body, whether it ended cleanly (ResponseEnd with no error), and the
    /// ResponseBody frame count.
    fn reassemble_for(frames: &[TunnelFrame], req_id: u64) -> (Option<u16>, Option<String>, Vec<u8>, bool, usize) {
        let mut status = None;
        let mut content_type = None;
        let mut body = Vec::new();
        let mut ended = false;
        let mut body_frames = 0;
        for frame in frames {
            match frame {
                TunnelFrame::ResponseHead { status: s, req_id: r, headers } if *r == req_id => {
                    status = Some(*s);
                    content_type = headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.clone());
                }
                TunnelFrame::ResponseBody { req_id: r, chunk } if *r == req_id => {
                    body.extend_from_slice(&chunk.0);
                    body_frames += 1;
                }
                TunnelFrame::ResponseEnd { req_id: r, error: None } if *r == req_id => ended = true,
                _ => {}
            }
        }
        (status, content_type, body, ended, body_frames)
    }

    #[tokio::test]
    async fn one_shot_request_round_trips() {
        let (herald, key, conv, capture) = setup().await;
        send_request(&herald, &key, &conv, 1, 1, "GET", "/oneshot").await;
        let frames = drain_frames(&capture, &key).await;
        let (status, content_type, body, ended, _) = reassemble_for(&frames, 1);
        assert_eq!(status, Some(200));
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(body, br#"{"ok":true}"#);
        assert!(ended, "a one-shot response must end with ResponseEnd");
    }

    #[tokio::test]
    async fn daemon_error_status_is_forwarded() {
        let (herald, key, conv, capture) = setup().await;
        send_request(&herald, &key, &conv, 1, 2, "GET", "/missing").await;
        let frames = drain_frames(&capture, &key).await;
        let (status, _, _, ended, _) = reassemble_for(&frames, 2);
        assert_eq!(
            status,
            Some(404),
            "the proxy must forward the daemon status, not enforce success"
        );
        assert!(ended);
    }

    #[tokio::test]
    async fn large_body_is_chunked_across_frames() {
        let (herald, key, conv, capture) = setup().await;
        send_request(&herald, &key, &conv, 1, 3, "GET", "/big").await;
        let frames = drain_frames(&capture, &key).await;
        let (status, _, body, ended, body_frames) = reassemble_for(&frames, 3);
        assert_eq!(status, Some(200));
        assert_eq!(body.len(), 100_000);
        assert!(body.iter().all(|b| *b == b'x'));
        assert!(ended);
        assert!(
            body_frames > 1,
            "a >CHUNK_CAP body must be split across ResponseBody frames, got {body_frames}"
        );
    }

    #[tokio::test]
    async fn streamed_body_is_reassembled_verbatim() {
        let (herald, key, conv, capture) = setup().await;
        send_request(&herald, &key, &conv, 1, 4, "GET", "/stream").await;
        let frames = drain_frames(&capture, &key).await;
        let (status, _, body, ended, _) = reassemble_for(&frames, 4);
        assert_eq!(status, Some(200));
        assert_eq!(body, b"chunk-A-chunk-B-chunk-C-");
        assert!(ended);
    }

    #[tokio::test]
    async fn concurrent_requests_multiplex_independently() {
        // Two requests with distinct req_ids run concurrently; their reply
        // frames interleave on the shared conversation stream and must be
        // demultiplexable by req_id, each reassembling to its own response.
        let (herald, key, conv, capture) = setup().await;
        tokio::join!(
            send_request(&herald, &key, &conv, 1, 100, "GET", "/oneshot"),
            send_request(&herald, &key, &conv, 2, 200, "GET", "/big"),
        );
        let frames = drain_frames(&capture, &key).await;
        let (status_a, _, body_a, ended_a, _) = reassemble_for(&frames, 100);
        let (status_b, _, body_b, ended_b, _) = reassemble_for(&frames, 200);
        assert_eq!(status_a, Some(200));
        assert_eq!(body_a, br#"{"ok":true}"#);
        assert!(ended_a);
        assert_eq!(status_b, Some(200));
        assert_eq!(body_b.len(), 100_000);
        assert!(ended_b);
    }

    #[test]
    fn sanitize_drops_auth_and_hop_by_hop_headers() {
        let input = vec![
            ("Authorization".into(), "Bearer client-token".into()),
            ("Host".into(), "evil.example".into()),
            ("Content-Length".into(), "3".into()),
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "*/*".into()),
        ];
        let out = sanitize_headers(input);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Content-Type", "Accept"]);
    }

    #[test]
    fn safe_path_rejects_scheme_and_requires_root() {
        // A bare path rooted at the daemon is safe.
        assert!(is_safe_path("/agents/a/events"));
        // `//evil` is not a scheme: prepended to the daemon URL it is just a path
        // (`http://host//evil`), so it is allowed.
        assert!(is_safe_path("//evil.example"));
        // A scheme is the only way to redirect the request off the daemon.
        assert!(!is_safe_path("http://evil.example/agents"));
        // Relative paths are rejected (the proxy speaks paths, not relative refs).
        assert!(!is_safe_path("relative/path"));
    }
}
