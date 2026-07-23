//! Async HTTP client for the kallip-lesche data-plane relay.
//!
//! Three surfaces, all authenticated with the tagma's `sk-tagma-` bearer:
//! - [`LescheClient::post_envelope`] — post an agent envelope, retrying on 503.
//! - [`LescheClient::post_key_exchange_response`] — post a KEX response.
//! - [`LescheClient::open_tunnel`] — open the long-lived herald tunnel SSE and
//!   yield parsed [`HeraldInbound`] events.
//!
//! # Two HTTP clients (load-bearing)
//!
//! The builder constructs two `reqwest::Client`s: one with a 30 s total timeout
//! for the request/reply POSTs, and one with **no total timeout** for the tunnel
//! stream. `reqwest`'s `.timeout()` is a whole-response deadline that also
//! covers the streaming body, so any finite value would kill the long-lived
//! tunnel SSE mid-flight. Do not collapse them into one client.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::StreamExt;
use kallip_agora_common::control::KeyExchangeResponse;
use kallip_agora_common::herald::HeraldInbound;
use kallip_agora_common::ids::{ConversationId, TagmaId};
use kallip_agora_common::message::Envelope;
use kallip_agora_common::proof::tunnel_transcript;
use kallip_e2ee::DeviceKey;

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

    /// Open the herald tunnel SSE and return a stream of parsed inbound events.
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
    ) -> Result<impl futures_core::Stream<Item = Result<HeraldInbound>> + use<>> {
        let url = self.url("/v1/herald/tunnel");
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
}

/// Drive the tunnel SSE: reassemble `\n\n`-framed event blocks from the byte
/// stream, concatenate their `data:` lines, and yield each as a parsed
/// [`HeraldInbound`].
///
/// Two error classes, handled differently (mirroring the original herald
/// reader): a malformed JSON *event* is yielded as an `Err` item and the stream
/// keeps draining (one bad frame must not tear down the tunnel); a chunk read or
/// UTF-8 failure is connection-level, so it is yielded and the stream ends,
/// letting the caller reconnect.
fn tunnel_stream(
    resp: reqwest::Response,
) -> impl futures_core::Stream<Item = Result<HeraldInbound>> {
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
                    match serde_json::from_str::<HeraldInbound>(&data) {
                        Ok(inbound) => yield Ok(inbound),
                        // One bad event: report it but keep draining the stream.
                        Err(e) => yield Err(anyhow::Error::new(e).context("invalid herald inbound JSON")),
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
            None => reqwest::Client::builder().build()?,
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
    use kallip_agora_common::bytes::Ciphertext;
    use kallip_agora_common::ids::{ConversationId, TagmaId, TraceId};
    use kallip_agora_common::message::{Envelope, Participant};
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
            conversation_id: ConversationId::for_tagma(&tagma_id),
            sender: Participant::Agent { tagma_id },
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
    async fn open_tunnel_sends_proof_headers_and_parses_event() {
        let server = MockServer::start().await;
        // The tunnel body: one SSE event carrying a KeyExchange HeraldInbound.
        let body = "data: {\"kind\":\"key_exchange\",\"conversation_id\":\"c1\",\"init\":{\"ephemeral_public\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"}}\n\n";
        Mock::given(method("GET"))
            .and(path("/v1/herald/tunnel"))
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
            HeraldInbound::KeyExchange {
                conversation_id, ..
            } => {
                assert_eq!(conversation_id.as_ref(), "c1");
            }
            HeraldInbound::Envelope { .. } => panic!("expected KeyExchange"),
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
            .and(path("/v1/herald/tunnel"))
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
}
