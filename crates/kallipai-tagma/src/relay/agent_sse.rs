//! The agent SSE carrier loop: connect, drain, dispatch.
//!
//! The tagma's delivery carrier for room and direct-session envelopes: a
//! bearer-only stream (no device proof per connect) held beside the
//! device-proof tunnel. An Envelope rides the same three-way routing
//! machine the tunnel feed uses (`handle_user_op`, which routes by the
//! channel-id value domain: joined room, direct session, or bilateral
//! 1:1); a Wake re-polls the joined-rooms cache outside the pump cadence.
//! The tunnel keeps the control frames (key exchange, the manage relay,
//! owner-subscription hints) and the bilateral envelope feed.

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::FutureExt;
use kallipai_lesche_common::event::LescheAgentEvent;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::RelayHandle;

/// Reconnect backoff bounds: retry quickly after a clean server-side close,
/// capped so a lesche that refuses connections cannot busy-loop the tagma.
/// Exponential (unlike the tunnel's fixed 2 s retry) because frames dropped
/// while this carrier is down are recovered by pull (history re-pull and the
/// room pump's immediate first tick on reconnect), not by hot reconnecting:
/// the lesche buffers nothing either way.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub(crate) async fn run(handle: RelayHandle, shutdown: CancellationToken) {
    let tagma_id = handle.tagma_id().clone();
    let mut backoff = BACKOFF_MIN;
    loop {
        let stats = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!(tagma = %tagma_id, "agent SSE carrier stopped");
                return;
            }
            stats = drain_once(&handle) => stats,
        };
        if stats.connected {
            backoff = BACKOFF_MIN;
        }
        info!(
            tagma = %tagma_id,
            connected = stats.connected,
            envelopes = stats.envelopes,
            wakes = stats.wakes,
            "agent SSE carrier connection ended"
        );
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!(tagma = %tagma_id, "agent SSE carrier stopped");
                return;
            }
            _ = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// One connection cycle: open the stream, dispatch every frame to its
/// routing tail, and drain to its end, counting frames by kind.
async fn drain_once(handle: &RelayHandle) -> CarrierStats {
    let tagma_id = handle.tagma_id().clone();
    let client = handle.lesche_client();
    let mut stats = CarrierStats::default();
    let stream = match client.open_agent_events(&tagma_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(tagma = %tagma_id, "agent SSE open failed: {e:#}");
            return stats;
        }
    };
    stats.connected = true;
    let mut stream = std::pin::pin!(stream);
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            Ok(LescheAgentEvent::Envelope { envelope }) => {
                // Same panic boundary as the tunnel's envelope dispatch: a
                // panic that escapes `handle_user_op`'s inner req_id-aware
                // boundary can only be logged here (no reply is possible
                // without a parsed req_id).
                if AssertUnwindSafe(handle.handle_user_op(envelope))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    error!("agent SSE op dispatch panicked past the inner boundary");
                }
                stats.envelopes += 1;
            }
            Ok(LescheAgentEvent::Wake) => {
                // Membership nudged: re-poll so the joined-rooms cache warms
                // before the next room envelope arrives (a just-added tagma
                // is blind until this poll lands). The Wake carries no
                // payload -- the sweep re-fetches the full list.
                handle.poll_rooms().await;
                stats.wakes += 1;
            }
            Err(e) => {
                tracing::warn!(tagma = %tagma_id, "agent SSE frame error: {e:#}");
            }
        }
    }
    stats
}

#[derive(Default)]
struct CarrierStats {
    connected: bool,
    envelopes: u64,
    wakes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    use kallipai_archeion_common::bytes::Ciphertext;
    use kallipai_archeion_common::ids::TagmaId;
    use kallipai_archeion_common::ids::{ChannelId, ParticipantId, ParticipantKind, TraceId};
    use kallipai_common::agentid::AgentId;
    use kallipai_e2ee::DeviceKey;
    use kallipai_lesche_client::LescheClient;
    use kallipai_lesche_common::message::{Envelope, Participant};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_envelope() -> Envelope {
        Envelope {
            channel_id: ChannelId::from("c-1".to_string()),
            sender: Participant {
                id: ParticipantId::for_user(&kallipai_archeion_common::ids::UserId::from(
                    "owner".to_string(),
                )),
                kind: ParticipantKind::Human,
                handle: "owner".into(),
                tagma_id: None,
            },
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
            ciphertext: Ciphertext(vec![0u8; 16]),
        }
    }

    /// The minimal dispatch-bearing handle: a real client aimed at the mock,
    /// a dangling `Weak<AppState>` (an Envelope dispatch reaches
    /// `handle_user_op`, which takes the shutdown-drop path and returns
    /// quietly), and a generated device key.
    fn handle(server: &MockServer, tagma: &str) -> RelayHandle {
        RelayHandle::new(
            LescheClient::builder(&server.uri(), "sk-tagma-test")
                .build()
                .unwrap(),
            "test".to_string(),
            TagmaId::from(tagma.to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            AgentId::from("root".to_string()),
            std::sync::Weak::new(),
        )
    }

    /// One cycle: an envelope and a wake arrive; the envelope is handed to
    /// the routing tail (the dangling state takes the quiet-drop path), and
    /// the wake re-polls `list_my_rooms` -- proven by the rooms GET the
    /// mock server received.
    #[tokio::test]
    async fn drain_once_dispatches_frames_and_counts() {
        let server = MockServer::start().await;
        let frame1 = serde_json::to_string(&LescheAgentEvent::Envelope {
            envelope: sample_envelope(),
        })
        .unwrap();
        let frame2 = serde_json::to_string(&LescheAgentEvent::Wake).unwrap();
        let body = format!("data: {frame1}\n\ndata: {frame2}\n\n");
        Mock::given(method("GET"))
            .and(path("/tagmata/t-1/events"))
            .and(header("authorization", "Bearer sk-tagma-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/tagmata/t-1/rooms"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;
        let h = handle(&server, "t-1");
        let stats = drain_once(&h).await;
        assert!(stats.connected);
        assert_eq!(stats.envelopes, 1);
        assert_eq!(stats.wakes, 1);
        let rooms_polled = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().ends_with("/rooms"))
            .count();
        assert_eq!(rooms_polled, 1, "Wake re-polled list_my_rooms");
    }

    /// A refused connection reports not-connected (the reconnect loop backs
    /// off); a pre-cancelled shutdown token stops the loop immediately.
    #[tokio::test]
    async fn open_failure_reports_disconnected_and_shutdown_stops() {
        let server = MockServer::start().await;
        let h = handle(&server, "t-2");
        let stats = drain_once(&h).await;
        assert!(!stats.connected);
        assert_eq!(stats.envelopes, 0);
        assert_eq!(stats.wakes, 0);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let stopped =
            tokio::time::timeout(std::time::Duration::from_secs(1), run(h, shutdown)).await;
        assert!(stopped.is_ok(), "run returns promptly on shutdown");
    }
}
