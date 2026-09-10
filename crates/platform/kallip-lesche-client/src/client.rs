//! Async HTTP client for the kallip-lesche data-plane relay.
//!
//! Authenticated with the tagma's `sk-tagma-` bearer:
//! - [`LescheClient::post_envelope`] — post an agent envelope, retrying on 503.
//! - [`LescheClient::post_key_exchange_response`] — post a KEX response.
//! - [`LescheClient::post_upstream`] — post a batch of plaintext metadata
//!   events (status, signal, projection) over the single upstream channel.
//! - [`LescheClient::open_tunnel`] — open the long-lived tunnel SSE and
//!   yield parsed [`TunnelInbound`] events.
//!
//! # Two HTTP clients (load-bearing)
//!
//! The builder constructs two `reqwest::Client`s: one with a 30 s total timeout
//! for the request/reply POSTs, and one with **no total timeout** for the tunnel
//! stream. `reqwest`'s `.timeout()` is a whole-response deadline that also
//! covers the streaming body, so any finite value would kill the long-lived
//! tunnel SSE mid-flight.
//!
//! Per-read `read_timeout` + TCP keepalive on the stream client are the
//! stall detectors: the server sends keepalive comments, so a healthy
//! stream always has bytes to read -- a silent gap means half-open.
//! Do not collapse the two clients into one.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::StreamExt;
use kallip_archeion_common::ids::{ChannelId, ConversationId, TagmaId};
use kallip_e2ee::DeviceKey;
use kallip_lesche_common::control::KeyExchangeResponse;
use kallip_lesche_common::direct::{DirectMessageView, DirectSessionId, DirectSessionView};
use kallip_lesche_common::event::UpstreamEvent;
use kallip_lesche_common::message::Envelope;
use kallip_lesche_common::proof::tunnel_transcript;
use kallip_lesche_common::rooms::{RoomId, TagmaRoomView};
use kallip_lesche_common::tunnel::ManageRestReply;
use kallip_lesche_common::tunnel::TunnelInbound;

struct Inner {
    base_url: String,
    /// Request/reply POSTs: carries a total timeout (natural request end).
    http_post: reqwest::Client,
    /// Long-lived tunnel stream: NO total timeout (see crate docs).
    http_stream: reqwest::Client,
    tagma_token: String,
}

/// Async HTTP client for the kallip-lesche data-plane relay.
#[derive(Clone)]
pub struct LescheClient {
    inner: Arc<Inner>,
}

/// One stored room message row, as returned by
/// [`LescheClient::fetch_room_messages`]. Mirrors the lesche `GET
/// /v1/rooms/{room}/messages` `StoredMessageView` shape; the row payload is the
/// plaintext `RoomMessage` JSON (rooms are server-readable).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RoomMessageView {
    pub seq: i64,
    pub sender: kallip_lesche_common::message::Participant,
    pub epoch: i64,
    pub ciphertext: kallip_archeion_common::bytes::Ciphertext,
    #[serde(with = "time::serde::iso8601")]
    pub created_at: time::OffsetDateTime,
}

/// Per-face live-subscriber counts echoed on an `/upstream` response (the
/// piggyback reconcile channel). Both are 0/1 today (one app
/// stream per owner; one projection channel per (user, tagma)) but stay
/// counts on the wire -- the caller derives booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct UpstreamFaceCounts {
    pub status: u64,
    pub projection: u64,
}

/// The `/upstream` response body. `faces` is absent from a legacy lesche:
/// the caller then reconciles nothing (the no-op leg of the mixed-version
/// reverse window).
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct UpstreamAck {
    #[serde(default)]
    pub faces: Option<UpstreamFaceCounts>,
}

/// A lesche HTTP failure with its status code, so a caller can distinguish a
/// member-gated 404 (unknown room / not a member) from a transport failure.
/// Only the room-surface methods return this; the rest of the client still uses
/// plain `anyhow::Error`.
#[derive(Debug, thiserror::Error)]
#[error("lesche {op} returned {status}")]
pub struct LescheHttpError {
    /// The operation that failed (for the message).
    op: &'static str,
    /// The HTTP status the lesche returned.
    pub status: reqwest::StatusCode,
}

impl LescheClient {
    /// Start building a [`LescheClient`]. `tagma_token` is the `sk-tagma-`
    /// bearer used on every data-plane call.
    pub fn builder(base_url: &str, tagma_token: impl Into<String>) -> LescheClientBuilder {
        LescheClientBuilder {
            base_url: base_url.trim_end_matches('/').to_owned(),
            tagma_token: tagma_token.into(),
            http_post: None,
            http_stream: None,
        }
    }

    /// Construct a client from environment variables: `KALLIP_LESCHE_URL`
    /// (default: `http://127.0.0.1:7200`) and `KALLIP_LESCHE_TAGMA_TOKEN`
    /// (required).
    pub fn from_env() -> Result<Self> {
        let url = std::env::var("KALLIP_LESCHE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7200".to_string());
        let token = std::env::var("KALLIP_LESCHE_TAGMA_TOKEN")
            .context("KALLIP_LESCHE_TAGMA_TOKEN required")?;
        Self::builder(&url, token).build()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// Post an agent envelope, retrying on 503 (app offline) with a bounded
    /// backoff (500ms, 1s, 2s, 4s, 8s, 16s ~= 31s total). A dropped reply is
    /// recovered by the app's host-history re-pull on reconnect, so the retry
    /// only rides out transient reconnects.
    pub async fn post_envelope(&self, conv_id: &ConversationId, envelope: &Envelope) -> Result<()> {
        const BACKOFF: [Duration; 6] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ];
        let url = self.url(&format!("/v1/conversations/{conv_id}/envelopes"));
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

    /// Post an agent envelope to a multi-member room: `/v1/rooms/{room_id}/
    /// envelopes`. The room route stores the payload and fans to live members,
    /// returning 202 ACCEPTED regardless of who is online (offline members pull on
    /// reconnect). A 202 means durably stored, so the only failure worth riding
    /// out is the lesche itself being transiently unavailable (503). Retried on
    /// 503 with the same bounded backoff as [`Self::post_envelope`]; any other
    /// non-2xx (404 unknown-room / non-member, 400 mismatch, 500 store) is a hard
    /// failure. A connect error also bails immediately (consistent with
    /// [`Self::post_envelope`]).
    pub async fn post_room_envelope(&self, room_id: &RoomId, envelope: &Envelope) -> Result<()> {
        const BACKOFF: [Duration; 6] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ];
        // The room route rejects (400) any envelope whose `channel_id` does
        // not equal the path room. The field is dual-purpose -- a bilateral
        // `ConversationId` (v5) on the 1:1 path, a `RoomId` (v4) on the room
        // path -- and a caller building a room envelope off a bilateral-shaped
        // helper can leave a stale bilateral id in it. Stamp it from `room_id`
        // here so no caller can address a room with the wrong id; the type
        // system will not catch it (both are UUID-string newtypes).
        let mut envelope = envelope.clone();
        envelope.channel_id = ChannelId::from(room_id.as_ref().to_string());
        let url = self.url(&format!("/v1/rooms/{room_id}/envelopes"));
        for wait in BACKOFF {
            let resp = self
                .inner
                .http_post
                .post(&url)
                .bearer_auth(&self.inner.tagma_token)
                .json(&envelope)
                .send()
                .await
                .context("lesche room POST failed")?;
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            // Retry only on 503 (lesche transiently unavailable); a 202 always
            // returns on success, so any other non-2xx is a hard failure.
            if status.as_u16() != 503 {
                return Err(LescheHttpError {
                    op: "room POST",
                    status,
                }
                .into());
            }
            tokio::time::sleep(wait).await;
        }
        anyhow::bail!("lesche room POST exhausted retries (lesche unavailable)")
    }

    /// Pull a room's message history: `GET /v1/rooms/{room_id}/messages?
    /// after_seq=&limit=`. Member-only on the relay; returns the stored rows
    /// (payload = plaintext `RoomMessage` JSON). A non-member / unknown room is
    /// a [`LescheHttpError`] with status 404. Not retried: a history pull is
    /// a fresh read, the next call supersedes a dropped one.
    pub async fn fetch_room_messages(
        &self,
        room_id: &RoomId,
        after_seq: Option<i64>,
        limit: Option<u64>,
    ) -> Result<Vec<RoomMessageView>> {
        let mut query = Vec::new();
        if let Some(a) = after_seq {
            query.push(("after_seq", a.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        let resp = self
            .inner
            .http_post
            .get(self.url(&format!("/v1/rooms/{room_id}/messages")))
            .query(&query)
            .bearer_auth(&self.inner.tagma_token)
            .send()
            .await
            .context("lesche room history GET failed")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LescheHttpError {
                op: "room history GET",
                status,
            }
            .into());
        }
        resp.json().await.context("decode room history")
    }

    /// List the calling tagma's rooms with each room's live membership + whether
    /// THIS tagma is the creator (`GET /v1/tagmata/{tagma_id}/rooms`). The
    /// tagma's room-membership pump polls this to refresh its joined-rooms
    /// routing cache. `tagma_id` should be the caller's own id (the route is
    /// self-only).
    pub async fn list_my_rooms(&self, tagma_id: &TagmaId) -> Result<Vec<TagmaRoomView>> {
        let resp = self
            .inner
            .http_post
            .get(self.url(&format!("/v1/tagmata/{tagma_id}/rooms")))
            .bearer_auth(&self.inner.tagma_token)
            .send()
            .await
            .context("list-my-rooms fetch failed")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("lesche list-my-rooms returned {status}");
        }
        resp.json().await.context("decode list-my-rooms response")
    }

    /// Post a key-exchange response for a conversation.
    pub async fn post_key_exchange_response(
        &self,
        conv_id: &ConversationId,
        response: &KeyExchangeResponse,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/v1/conversations/{conv_id}/key-exchange/response"
        ));
        let resp = self
            .inner
            .http_post
            .post(&url)
            .bearer_auth(&self.inner.tagma_token)
            .json(response)
            .send()
            .await
            .context("lesche POST failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("lesche POST returned {}", resp.status());
        }
        Ok(())
    }

    /// Reply to a manage-rest frame: the plaintext counterpart of the
    /// ManageRest tunnel frame. Not retried: the pending waiter on the
    /// lesche side times out and surfaces a 504 to the proxy caller, so a
    /// dropped reply degrades to an error, never to stale data.
    pub async fn post_manage_reply(&self, payload: &ManageRestReply) -> Result<()> {
        let url = self.url("/v1/tunnel/manage-reply");
        let resp = self
            .inner
            .http_post
            .post(&url)
            .bearer_auth(&self.inner.tagma_token)
            .json(payload)
            .send()
            .await
            .context("lesche POST failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("lesche POST returned {}", resp.status());
        }
        Ok(())
    }

    /// Post a batch of plaintext metadata events to the single upstream
    /// channel (`POST /v1/tagmata/{id}/upstream`): one authenticated request
    /// carries status, signal, and projection elements together, each
    /// demultiplexed server-side into the same fan logic the per-kind
    /// endpoints have always used. Best-effort like the per-kind calls it
    /// supersedes; a failure here is logged by the caller. Returns the
    /// [`UpstreamAck`] so the caller can reconcile its per-face gates
    /// against the lesche's piggybacked subscriber counts.
    pub async fn post_upstream(
        &self,
        tagma_id: &TagmaId,
        events: &[UpstreamEvent],
    ) -> Result<UpstreamAck> {
        let url = self.url(&format!("/v1/tagmata/{tagma_id}/upstream"));
        let resp = self
            .inner
            .http_post
            .post(&url)
            .bearer_auth(&self.inner.tagma_token)
            .json(&events)
            .send()
            .await
            .context("lesche POST failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("lesche POST returned {}", resp.status());
        }
        resp.json().await.context("decode upstream ack")
    }

    /// Open the tunnel SSE and return a stream of parsed inbound events.
    ///
    /// The reconnect proof (timestamp + signature over the tunnel transcript) is
    /// generated once per call and validated by the lesche against the tagma's
    /// monotonic high-water-mark. Callers MUST call `open_tunnel` fresh on each
    /// reconnect (do not cache/reuse the stream): the proof timestamp is
    /// single-use, and the lesche rejects timestamps that do not strictly
    /// advance the per-tagma marker.
    pub async fn open_tunnel(
        &self,
        device: &DeviceKey,
        tagma_id: &TagmaId,
    ) -> Result<impl futures_core::Stream<Item = Result<TunnelInbound>> + use<>> {
        let url = self.url("/v1/tunnel");
        let unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let proof = device.sign(&tunnel_transcript(tagma_id.as_ref(), unix_secs));
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
        Ok(tunnel_stream(resp))
    }
    // ------------------------------------------------------------------
    // Direct sessions (T↔T plaintext 1v1). Same wire discipline as the
    // room surface: opaque plaintext payload bytes, a sync 202 ack, and
    // member-gated 404s that never distinguish "unknown session" from
    // "not a member".

    /// Create-or-get the direct session between the calling tagma and
    /// `peer` (`POST /v1/direct-sessions`). Idempotent: the same pair lands
    /// on the same derived session whichever side calls. A 404 means
    /// unknown / cross-owner / not-usable peer -- the existence-oracle, so
    /// the caller cannot tell which.
    pub async fn create_direct_session(&self, peer: &TagmaId) -> Result<DirectSessionView> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            peer: &'a str,
        }
        let resp = self
            .inner
            .http_post
            .post(self.url("/v1/direct-sessions"))
            .bearer_auth(&self.inner.tagma_token)
            .json(&Body {
                peer: peer.as_ref(),
            })
            .send()
            .await
            .context("lesche direct-session POST failed")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LescheHttpError {
                op: "direct-session POST",
                status,
            }
            .into());
        }
        resp.json().await.context("decode direct-session view")
    }

    /// List the calling tagma's direct sessions
    /// (`GET /v1/direct-sessions`), each with the peer's identity.
    pub async fn list_direct_sessions(&self) -> Result<Vec<DirectSessionView>> {
        let resp = self
            .inner
            .http_post
            .get(self.url("/v1/direct-sessions"))
            .bearer_auth(&self.inner.tagma_token)
            .send()
            .await
            .context("lesche direct-session list GET failed")?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("lesche direct-session list returned {status}");
        }
        resp.json().await.context("decode direct-session list")
    }

    /// Send a plaintext `DirectMessage` envelope into the session
    /// (`POST /v1/direct-sessions/{id}/messages`), retrying on 503 with the
    /// room surface's bounded backoff. The channel is stamped from
    /// `session_id` here so no caller can address a session with a
    /// mismatched id (the route rejects any mismatch with a 400 anyway).
    pub async fn post_direct_session_envelope(
        &self,
        session_id: &DirectSessionId,
        envelope: &Envelope,
    ) -> Result<()> {
        const BACKOFF: [Duration; 6] = [
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ];
        let mut envelope = envelope.clone();
        envelope.channel_id = ChannelId::from(session_id.as_ref().to_string());
        let url = self.url(&format!("/v1/direct-sessions/{session_id}/messages"));
        for wait in BACKOFF {
            let resp = self
                .inner
                .http_post
                .post(&url)
                .bearer_auth(&self.inner.tagma_token)
                .json(&envelope)
                .send()
                .await
                .context("lesche direct-session message POST failed")?;
            let status = resp.status();
            if status.is_success() {
                return Ok(());
            }
            // Retry only on 503 (lesche transiently unavailable); a 202
            // always returns on success, so anything else is a hard failure.
            if status.as_u16() != 503 {
                return Err(LescheHttpError {
                    op: "direct-session message POST",
                    status,
                }
                .into());
            }
            tokio::time::sleep(wait).await;
        }
        anyhow::bail!("lesche direct-session POST exhausted retries (lesche unavailable)")
    }

    /// Pull the session's message history
    /// (`GET /v1/direct-sessions/{id}/messages?after_seq=&limit=`).
    /// Member-only on the relay; rows carry the plaintext `DirectMessage`
    /// JSON as opaque bytes. A non-member / unknown session is a
    /// [`LescheHttpError`] with status 404. Not retried: a history pull is a
    /// fresh read, the next call supersedes a dropped one.
    pub async fn fetch_direct_messages(
        &self,
        session_id: &DirectSessionId,
        after_seq: Option<i64>,
        limit: Option<u64>,
    ) -> Result<Vec<DirectMessageView>> {
        let mut query = Vec::new();
        if let Some(a) = after_seq {
            query.push(("after_seq", a.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        let resp = self
            .inner
            .http_post
            .get(self.url(&format!("/v1/direct-sessions/{session_id}/messages")))
            .query(&query)
            .bearer_auth(&self.inner.tagma_token)
            .send()
            .await
            .context("lesche direct-session history GET failed")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LescheHttpError {
                op: "direct-session history GET",
                status,
            }
            .into());
        }
        resp.json().await.context("decode direct-session history")
    }

    /// Advance the calling tagma's read cursor in the session
    /// (`PUT /v1/direct-sessions/{id}/read-cursor`). Clamp-on-write
    /// server-side: a stale write never moves the watermark backwards.
    pub async fn put_direct_read_cursor(
        &self,
        session_id: &DirectSessionId,
        last_read_seq: i64,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body {
            last_read_seq: i64,
        }
        let resp = self
            .inner
            .http_post
            .put(self.url(&format!("/v1/direct-sessions/{session_id}/read-cursor")))
            .bearer_auth(&self.inner.tagma_token)
            .json(&Body { last_read_seq })
            .send()
            .await
            .context("lesche direct-session cursor PUT failed")?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LescheHttpError {
                op: "direct-session cursor PUT",
                status,
            }
            .into());
        }
        Ok(())
    }
}

/// Drive the tunnel SSE: reassemble `\n\n`-framed event blocks from the byte
/// stream, concatenate their `data:` lines, and yield each as a parsed
/// [`TunnelInbound`].
///
/// Two error classes, handled differently (mirroring the original reader): a
/// malformed JSON *event* is yielded as an `Err` item and the stream keeps
/// draining (one bad frame must not tear down the tunnel); a chunk read or UTF-8
/// failure is connection-level, so it is yielded and the stream ends, letting
/// the caller reconnect.
fn tunnel_stream(
    resp: reqwest::Response,
) -> impl futures_core::Stream<Item = Result<TunnelInbound>> {
    async_stream::stream! {
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(anyhow::Error::new(e).context("tunnel chunk"));
                    return;
                }
            };
            match std::str::from_utf8(&chunk) {
                Ok(s) => buf.push_str(s),
                Err(e) => {
                    yield Err(anyhow::Error::new(e).context("non-utf8 SSE chunk"));
                    return;
                }
            }
            while let Some(idx) = buf.find("\n\n") {
                let event = buf[..idx].to_string();
                buf.drain(..=idx + 1);
                if let Some(data) = parse_data_payload(&event) {
                    match serde_json::from_str::<TunnelInbound>(&data) {
                        Ok(inbound) => yield Ok(inbound),
                        // One bad event: report it but keep draining the stream.
                        Err(e) => yield Err(anyhow::Error::new(e).context("invalid tunnel inbound JSON")),
                    }
                }
            }
        }
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

/// Build a [`LescheClient`] with optional HTTP-client overrides.
pub struct LescheClientBuilder {
    base_url: String,
    tagma_token: String,
    http_post: Option<reqwest::Client>,
    http_stream: Option<reqwest::Client>,
}

impl LescheClientBuilder {
    /// Override the request/reply POST client (default: 30 s total timeout).
    pub fn http_post_client(mut self, client: reqwest::Client) -> Self {
        self.http_post = Some(client);
        self
    }

    /// Override the long-lived tunnel-stream client (default: NO total timeout).
    /// The supplied client MUST NOT carry a total timeout, or the tunnel SSE
    /// will be killed mid-flight.
    pub fn http_stream_client(mut self, client: reqwest::Client) -> Self {
        self.http_stream = Some(client);
        self
    }

    /// Consume the builder and produce a [`LescheClient`].
    pub fn build(self) -> Result<LescheClient> {
        let http_post = match self.http_post {
            Some(c) => c,
            None => reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        };
        let http_stream = match self.http_stream {
            Some(c) => c,
            None => {
                // Stall detectors for the tunnel SSE: server keepalive
                // comments arrive every 15 s, so anything quieter than the
                // read window means half-open. No total timeout -- the stream
                // is meant to live as long as both ends do.
                reqwest::Client::builder()
                    .read_timeout(Duration::from_secs(90))
                    .tcp_keepalive(Duration::from_secs(15))
                    .build()?
            }
        };
        Ok(LescheClient {
            inner: Arc::new(Inner {
                base_url: self.base_url,
                http_post,
                http_stream,
                tagma_token: self.tagma_token,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_archeion_common::bytes::Ciphertext;
    use kallip_archeion_common::ids::{
        ConversationId, ParticipantId, ParticipantKind, TagmaId, TraceId,
    };
    use kallip_lesche_common::event::{AgentState, SignalEvent, TagmaStatusPayload};
    use kallip_lesche_common::message::{Envelope, Participant};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> LescheClient {
        LescheClient::builder(&server.uri(), "sk-tagma-test")
            .build()
            .unwrap()
    }

    fn sample_envelope(seq: u64) -> Envelope {
        let tagma_id = TagmaId::from("tagma-1".to_string());
        Envelope {
            channel_id: ChannelId::from(ConversationId::for_tagma(&tagma_id).to_string()),
            sender: Participant {
                id: ParticipantId::for_tagma(&tagma_id),
                kind: ParticipantKind::Agent,
                handle: "Tagma".into(),
                tagma_id: Some(tagma_id),
            },
            sequence_n: seq,
            trace_id: TraceId::from("trace-1".to_string()),
            timestamp: time::OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(vec![0u8; 12]),
        }
    }

    fn conv() -> ConversationId {
        ConversationId::for_tagma(&TagmaId::from("tagma-1".to_string()))
    }

    #[tokio::test]
    async fn post_envelope_retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        let conv = conv();
        Mock::given(method("POST"))
            .and(path(format!("/v1/conversations/{conv}/envelopes")))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v1/conversations/{conv}/envelopes")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server)
            .post_envelope(&conv, &sample_envelope(0))
            .await
            .expect("succeeds after one 503");
    }

    #[tokio::test]
    async fn post_envelope_bails_on_non_503() {
        let server = MockServer::start().await;
        let conv = conv();
        Mock::given(method("POST"))
            .and(path(format!("/v1/conversations/{conv}/envelopes")))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server)
            .post_envelope(&conv, &sample_envelope(0))
            .await
            .expect_err("401");
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    #[tokio::test]
    async fn post_room_envelope_posts_to_room_path() {
        // The room route returns 202 ACCEPTED (stored + fanned) regardless of
        // who is online; no 503 retry, just a single POST.
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/rooms/{room}/envelopes")))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        client(&server)
            .post_room_envelope(&room, &sample_envelope(0))
            .await
            .expect("accepted");
    }

    #[tokio::test]
    async fn post_room_envelope_bails_on_404() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/rooms/{room}/envelopes")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = client(&server)
            .post_room_envelope(&room, &sample_envelope(0))
            .await
            .expect_err("404");
        assert!(err.to_string().contains("404"), "got: {err}");
    }

    #[tokio::test]
    async fn post_room_envelope_stamps_channel_id_from_room() {
        // The room route rejects (400) an envelope whose channel_id does
        // not match the path room. The client must overwrite whatever the caller
        // set (here a bilateral conversation id) with the room id, so a caller
        // cannot address a room with a stale bilateral id.
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/rooms/{room}/envelopes")))
            .and(body_partial_json(
                serde_json::json!({ "channel_id": "room-1" }),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        // The sample envelope carries a bilateral conversation_id derived from a
        // tagma id -- NOT "room-1". The stamp must replace it.
        client(&server)
            .post_room_envelope(&room, &sample_envelope(0))
            .await
            .expect("accepted with stamped conversation_id");
    }

    #[tokio::test]
    async fn fetch_room_messages_gets_history_path_and_decodes_rows() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        // Participant is a struct wire type: `{ id, kind, handle }`.
        let body = serde_json::json!([{
            "seq": 3,
            "sender": {"id": "p-tagma-1", "kind": "agent", "handle": "Tagma"},
            "epoch": 1,
            "ciphertext": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "created_at": "2026-08-02T00:00:00Z",
        }]);
        Mock::given(method("GET"))
            .and(path("/v1/rooms/room-1/messages"))
            .and(query_param("after_seq", "2"))
            .and(query_param("limit", "10"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let rows = client(&server)
            .fetch_room_messages(&room, Some(2), Some(10))
            .await
            .expect("history fetched");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 3);
        assert_eq!(rows[0].epoch, 1);
        assert_eq!(rows[0].ciphertext.0.len(), 32);
    }

    #[tokio::test]
    async fn create_direct_session_posts_the_peer_and_decodes_the_view() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let peer = TagmaId::from("tagma-2".to_string());
        Mock::given(method("POST"))
            .and(path("/v1/direct-sessions"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .and(body_partial_json(serde_json::json!({ "peer": "tagma-2" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": "s-derived",
                "peer": { "tagma_id": "tagma-2", "handle": "tagma-2@alice" },
                "created_at": "2026-09-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        let view = client(&server)
            .create_direct_session(&peer)
            .await
            .expect("created");
        assert_eq!(view.session_id.as_ref(), "s-derived");
        assert_eq!(view.peer.tagma_id, peer);
    }

    #[tokio::test]
    async fn list_direct_sessions_decodes_the_view_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/direct-sessions"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "session_id": "s-1",
                    "peer": { "tagma_id": "tagma-2", "handle": "tagma-2@alice" },
                    "created_at": "2026-09-01T00:00:00Z",
                }
            ])))
            .mount(&server)
            .await;
        let rows = client(&server)
            .list_direct_sessions()
            .await
            .expect("listed");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peer.tagma_id.as_ref(), "tagma-2");
    }

    #[tokio::test]
    async fn post_direct_session_envelope_stamps_channel_id_from_session() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        let session = DirectSessionId::from("s-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/direct-sessions/{session}/messages")))
            .and(body_partial_json(
                serde_json::json!({ "channel_id": "s-1" }),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        // The sample envelope carries a bilateral conversation id -- NOT
        // "s-1". The stamp must replace it (the route 400s any mismatch, so
        // a caller cannot address a session with a stale id).
        client(&server)
            .post_direct_session_envelope(&session, &sample_envelope(0))
            .await
            .expect("accepted with stamped channel_id");
    }

    #[tokio::test]
    async fn post_direct_session_envelope_retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("s-1".to_string());
        let url = format!("/v1/direct-sessions/{session}/messages");
        Mock::given(method("POST"))
            .and(path(url.clone()))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(url))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        client(&server)
            .post_direct_session_envelope(&session, &sample_envelope(0))
            .await
            .expect("succeeds after one 503");
    }

    #[tokio::test]
    async fn post_direct_session_envelope_bails_on_non_503() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("s-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/direct-sessions/{session}/messages")))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server)
            .post_direct_session_envelope(&session, &sample_envelope(0))
            .await
            .expect_err("401");
        assert!(err.to_string().contains("401"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_direct_messages_sends_query_and_decodes_rows() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        let session = DirectSessionId::from("s-1".to_string());
        // The direct row has no epoch; the sender carries the deep-link
        // tagma_id the room row does not.
        let body = serde_json::json!([{
            "seq": 4,
            "sender": {"id": "p-tagma-1", "kind": "agent", "handle": "Tagma", "tagma_id": "tagma-1"},
            "ciphertext": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "created_at": "2026-08-02T00:00:00Z",
        }]);
        Mock::given(method("GET"))
            .and(path("/v1/direct-sessions/s-1/messages"))
            .and(query_param("after_seq", "3"))
            .and(query_param("limit", "10"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let rows = client(&server)
            .fetch_direct_messages(&session, Some(3), Some(10))
            .await
            .expect("history fetched");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq, 4);
        assert_eq!(rows[0].ciphertext.0.len(), 32);
        assert_eq!(
            rows[0].sender.tagma_id.as_ref().map(|t| t.as_ref()),
            Some("tagma-1")
        );
    }

    #[tokio::test]
    async fn put_direct_read_cursor_puts_the_session_cursor_path() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("s-1".to_string());
        Mock::given(method("PUT"))
            .and(path(format!("/v1/direct-sessions/{session}/read-cursor")))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server)
            .put_direct_read_cursor(&session, 7)
            .await
            .expect("cursor advanced");
    }

    #[tokio::test]
    async fn post_upstream_posts_batch_to_upstream_path() {
        let server = MockServer::start().await;
        let tagma_id = TagmaId::from("tagma-1".to_string());
        Mock::given(method("POST"))
            .and(path(format!("/v1/tagmata/{tagma_id}/upstream")))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"applied": 2, "faces": {"status": 1, "projection": 0}}),
            ))
            .mount(&server)
            .await;
        let ack = client(&server)
            .post_upstream(
                &tagma_id,
                &[
                    UpstreamEvent::Status(TagmaStatusPayload {
                        root_state: AgentState::Busy,
                        subagents_total: 1,
                        subagents_active: 1,
                        token_budget: 9,
                        token_consumed: 3,
                        token_budget_unlimited: false,
                    }),
                    UpstreamEvent::Signal(SignalEvent::Idle),
                ],
            )
            .await
            .expect("200 ok");
        assert_eq!(
            ack.faces,
            Some(UpstreamFaceCounts {
                status: 1,
                projection: 0,
            }),
            "piggybacked face counts surface to the caller",
        );
    }

    #[tokio::test]
    async fn open_tunnel_sends_proof_headers_and_parses_event() {
        let server = MockServer::start().await;
        // The tunnel body: one SSE event carrying a KeyExchange TunnelInbound.
        let body = "data: {\"kind\":\"key_exchange\",\"conversation_id\":\"c1\",\"init\":{\"ephemeral_public\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"}}\n\n";
        Mock::given(method("GET"))
            .and(path("/v1/tunnel"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let device = DeviceKey::generate();
        let tagma_id = TagmaId::from("tagma-1".to_string());
        let stream = client(&server)
            .open_tunnel(&device, &tagma_id)
            .await
            .expect("open");
        let collected: Vec<_> = stream.collect::<Vec<_>>().await;
        assert_eq!(collected.len(), 1);
        assert!(collected[0].is_ok(), "parsed one event");
        match collected[0].as_ref().unwrap() {
            TunnelInbound::KeyExchange {
                conversation_id, ..
            } => {
                assert_eq!(conversation_id.as_ref(), "c1");
            }
            TunnelInbound::Envelope { .. } => panic!("expected KeyExchange"),
            TunnelInbound::Wake => panic!("expected KeyExchange"),
            TunnelInbound::ManageRest { .. } => panic!("expected KeyExchange"),
            TunnelInbound::OwnerSub { .. } => panic!("expected KeyExchange"),
        }
    }

    /// A malformed JSON event must NOT tear down the tunnel: it surfaces as an
    /// `Err` item and the stream keeps draining, so the next valid event still
    /// arrives. (Regression guard: an earlier `try_stream!` + `?` version
    /// terminated the whole stream on the first bad frame.)
    #[tokio::test]
    async fn open_tunnel_survives_bad_json_event() {
        let server = MockServer::start().await;
        let good = "data: {\"kind\":\"key_exchange\",\"conversation_id\":\"c1\",\"init\":{\"ephemeral_public\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"}}\n\n";
        // A malformed event followed by a valid one.
        let body = format!("data: not-json\n\n{good}");
        Mock::given(method("GET"))
            .and(path("/v1/tunnel"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let device = DeviceKey::generate();
        let tagma_id = TagmaId::from("tagma-1".to_string());
        let collected: Vec<_> = client(&server)
            .open_tunnel(&device, &tagma_id)
            .await
            .expect("open")
            .collect::<Vec<_>>()
            .await;
        // The bad frame yields Err, the good frame yields Ok -- both arrive, in
        // order, and the stream is not cut short by the bad frame.
        assert_eq!(collected.len(), 2, "stream drained past the bad frame");
        assert!(collected[0].is_err(), "bad frame surfaces as Err");
        assert!(collected[1].is_ok(), "good frame still arrives");
    }

    /// Keepalive comment frames (": like this") must be silently skipped:
    /// no item, no stream end, and the following real event still parses.
    /// This is the regression guard for the server-side keep_alive added
    /// alongside these very clients.
    #[tokio::test]
    async fn open_tunnel_skips_keepalive_comments() {
        let server = MockServer::start().await;
        let good = "data: {\"kind\":\"wake\"}\n\n";
        // Comment block first, then a real event.
        let body = format!(": keepalive\n\n{good}");
        Mock::given(method("GET"))
            .and(path("/v1/tunnel"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let device = DeviceKey::generate();
        let tagma_id = TagmaId::from("tagma-1".to_string());
        let collected: Vec<_> = client(&server)
            .open_tunnel(&device, &tagma_id)
            .await
            .expect("open")
            .collect::<Vec<_>>()
            .await;
        assert_eq!(collected.len(), 1, "comment frame yields no item");
        assert!(matches!(collected[0], Ok(TunnelInbound::Wake)));
    }
}
