//! The upstream flusher: the single wire writer for the tagma's plaintext
//! metadata uplink.
//!
//! One task subscribes the status, projection, and signal bus topics and
//! serializes drained frames into batched `POST /tagmata/{id}/upstream`
//! requests. This is where the drivers' former per-driver POST/PUT arms
//! converged: the drivers publish typed snapshots, the flusher alone speaks
//! `UpstreamEvent` on the wire (the enum never enters the bus).
//!
//! The status face carries its own subscriber gate
//! here, at the send decision -- the only place a gate can suppress the
//! wire without touching the bus (the in-process SSE assembler keeps
//! receiving every StatusSnapshot regardless). While gated, a
//! drained status snapshot RETAINS in its keep-last slot instead of
//! reaching the wire; the open edge (an `OwnerSub` frame, a piggybacked
//! count, or a tunnel re-learn) wakes the flusher via
//! `status_gate_notify` and the retained full snapshot POSTs immediately
//! -- resume is the keep-last structure itself, nothing partial exists.
//! The gate defaults OPEN: a lesche that never signals the gate
//! degrades to the shipped always-push semantics, while an erroneously
//! open gate self-corrects within one push's piggyback.
//!
//! Per-topic policy (the parameterized strategy):
//! - status / projection: keep-last. A failed POST retains the newest
//!   snapshot per topic and retries on a short tick — snapshots are
//!   idempotent latest-wins server-side, so a retained retry can never
//!   resurrect stale state. Upstream republication is change-gated (the
//!   pump posts only when a capture differs from the last); the pump's
//!   fallback ticker, not a forced resend, bounds staleness.
//! - signal: fire-and-forget, never retained. A signal is a transient
//!   transition the next event supersedes; retrying one after a failure
//!   would replay a stale presence flip.
//!
//! Same-topic duplicates collapse per batch: the relay and direct status
//! drivers both publish (their start/stop triggers are genuinely
//! different), and keep-last per drain window sends one `Status` event per
//! POST even when both producers fired — the dual-producer
//! wire-side completion.

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use kallip_lesche_client::UpstreamFaceCounts;
use kallip_lesche_common::event::UpstreamEvent;

use crate::bus::{ProjectionSnapshot, SignalFrame, StatusSnapshot};

use super::RelayHandle;

impl RelayHandle {
    /// Run the upstream flusher until cancelled or the bus closes. Started
    /// and stopped with the tunnel session, alongside the snapshot pumps:
    /// the upstream POST needs the KEX'd client this handle owns.
    pub(super) async fn run_upstream_flusher(self, cancel: CancellationToken) {
        let Some(state) = self.inner.state.upgrade() else {
            return; // startup raced shutdown
        };
        let (mut status_rx, mut projection_rx, mut signal_rx) = match (
            state.bus.subscribe::<StatusSnapshot>(),
            state.bus.subscribe::<ProjectionSnapshot>(),
            state.bus.subscribe::<SignalFrame>(),
        ) {
            (Ok(s), Ok(p), Ok(g)) => (s, p, g),
            _ => {
                warn!("upstream flusher: bus topic missing; not starting");
                return;
            }
        };
        drop(state); // the receivers keep the bus alive; drop our strong ref

        // Latest unsent snapshot per topic (keep-last across drain windows
        // when a POST fails). Signals are never retained.
        let mut pending_status: Option<StatusSnapshot> = None;
        let mut pending_projection: Option<ProjectionSnapshot> = None;
        let mut signals: Vec<kallip_common::protocol::SignalEvent> = Vec::new();
        let mut retry: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

        loop {
            // Drain: keep-last per snapshot topic, queue signals in order.
            loop {
                match status_rx.try_recv() {
                    Ok(frame) => pending_status = Some(frame),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        warn!(lagged = n, "upstream flusher lagged status frames")
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        debug!("upstream flusher: status topic closed");
                        return;
                    }
                }
            }
            loop {
                match projection_rx.try_recv() {
                    Ok(frame) => pending_projection = Some(frame),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        warn!(lagged = n, "upstream flusher lagged projection frames")
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        debug!("upstream flusher: projection topic closed");
                        return;
                    }
                }
            }
            loop {
                match signal_rx.try_recv() {
                    Ok(SignalFrame(signal)) => signals.push(signal),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        // Ephemeral transitions: nothing to replay; the next
                        // signal supersedes.
                        warn!(lagged = n, "upstream flusher lagged signal frames")
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        debug!("upstream flusher: signal topic closed");
                        return;
                    }
                }
            }

            // Serialize the batch: retained snapshots first (retry order),
            // then fresh signals. Same-topic duplicates collapsed above.
            let mut batch: Vec<UpstreamEvent> = Vec::new();
            // Status gate: take the retained snapshot into the batch only
            // when the gate is open (keep-last-while-gated). The
            // read is post-drain, pre-batch: the gate never touches the bus,
            // so draining continues while gated and no receiver lag builds.
            if self
                .inner
                .status_push_active
                .load(std::sync::atomic::Ordering::Relaxed)
                && let Some(snapshot) = pending_status.take()
            {
                batch.push(UpstreamEvent::Status(snapshot.0));
            }
            if let Some(snapshot) = pending_projection.take() {
                batch.push(UpstreamEvent::Projection(Box::new(snapshot.0)));
            }
            batch.extend(signals.drain(..).map(UpstreamEvent::Signal));

            if !batch.is_empty() {
                let flush = self
                    .inner
                    .client
                    .post_upstream(&self.inner.tagma_id, &batch);
                // Cancel-select'd: a tunnel-down aborts the in-flight POST
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    r = flush => match r {
                    Ok(ack) => {
                        retry = None;
                        // Piggyback reconcile: the response's
                        // per-face counts are the lesche's freshest truth;
                        // a diverging face flips through the same handlers
                        // the tunnel dispatch uses (flip + notify-if-open).
                        // An absent field (no piggyback counts) reconciles as a
                        // no-op -- the mixed-version reverse window.
                        if let Some(faces) = ack.faces {
                            self.reconcile_faces(faces).await;
                        }
                    }
                        Err(e) => {
                            warn!(
                                tagma = %self.inner.tagma_id,
                                "upstream flush failed: {e:#}; retaining snapshots"
                            );
                            // Restore the snapshot elements for retry;
                            // signals stay dropped (fire-and-forget).
                            for event in batch {
                                match event {
                                    UpstreamEvent::Status(payload) => {
                                        pending_status = Some(StatusSnapshot(payload));
                                    }
                                    UpstreamEvent::Projection(snapshot) => {
                                        pending_projection = Some(ProjectionSnapshot(*snapshot));
                                    }
                                    UpstreamEvent::Signal(_) => {}
                                }
                            }
                            let retry_ms = self
                                .inner
                                .flusher_retry_ms
                                .load(std::sync::atomic::Ordering::Relaxed);
                            retry = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                                retry_ms,
                            ))));
                        }
                    },
                }
            }

            // Wait for the next wake: any topic frame, the armed retry tick,
            // or shutdown. A recv'd frame is stashed and the loop drains the
            // rest non-blocking.
            let wait_retry = async {
                match retry.as_mut() {
                    Some(sleep) => sleep.as_mut().await,
                    None => std::future::pending().await,
                }
            };
            // The gate's wake edge: pinned and enabled BEFORE the select
            // so a flip during the drain window cannot be missed;
            // the 2 s cadence backstop bounds any residual wake loss.
            let gate_wake = self.inner.status_gate_notify.notified();
            tokio::pin!(gate_wake);
            gate_wake.as_mut().enable();
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                recv = status_rx.recv() => match recv {
                    Ok(frame) => pending_status = Some(frame),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "upstream flusher lagged status frames")
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("upstream flusher: status topic closed");
                        return;
                    }
                },
                recv = projection_rx.recv() => match recv {
                    Ok(frame) => pending_projection = Some(frame),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "upstream flusher lagged projection frames")
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("upstream flusher: projection topic closed");
                        return;
                    }
                },
                recv = signal_rx.recv() => match recv {
                    Ok(SignalFrame(signal)) => signals.push(signal),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "upstream flusher lagged signal frames")
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!("upstream flusher: signal topic closed");
                        return;
                    }
                },
                _ = wait_retry => {}
                _ = gate_wake => {}
            }
        }
    }

    /// Ensure the upstream flusher is running. Idempotent, like the pump
    /// starts: a no-op when one is already live for this tunnel session.
    pub(super) async fn start_upstream_flusher(&self) {
        self.inner
            .upstream_flusher
            .get_or_spawn(|cancel| self.clone().run_upstream_flusher(cancel))
            .await;
    }

    /// Stop and await the upstream flusher if it is running, clearing the
    /// slot so a later start installs a fresh one against the new session.
    pub(super) async fn stop_upstream_flusher(&self) {
        self.inner.upstream_flusher.stop_and_await().await;
    }

    /// Consume the status face of an `OwnerSub` frame (the tunnel dispatch
    /// and the piggyback reconcile both land here). Swaps the gate; on a
    /// false -> true flip, wakes the flusher so the retained full snapshot
    /// POSTs immediately instead of waiting out the cadence.
    /// Idempotent on a same-value re-send -- the reconnect `true -> true`
    /// re-learn is a no-op, mirroring [`RelayHandle::handle_projection_hint`].
    pub(super) async fn handle_status_sub(&self, active: bool) {
        let was = self
            .inner
            .status_push_active
            .swap(active, std::sync::atomic::Ordering::Relaxed);
        if was == active {
            debug!(face = "status", active, "status gate unchanged");
            return;
        }
        debug!(face = "status", active, "status gate flipped");
        if active {
            self.inner.status_gate_notify.notify_one();
        }
    }

    /// Reconcile both faces against the `/upstream` response's piggybacked
    /// subscriber counts: the counts are the lesche's freshest
    /// per-face truth, so a diverging face flips through the same handlers
    /// the tunnel dispatch uses. The projection face rides the same lane
    /// without disturbing its pump semantics (a flip is exactly a hint).
    async fn reconcile_faces(&self, faces: UpstreamFaceCounts) {
        self.handle_status_sub(faces.status > 0).await;
        self.handle_projection_hint(faces.projection > 0).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ProjectionSnapshot as BusProjection, StatusSnapshot as BusStatus};
    use crate::state::SharedState;
    use crate::test_helpers::make_state;
    use kallip_archeion_common::ids::TagmaId;
    use kallip_common::protocol::{AgentState, SignalEvent};
    use kallip_e2ee::DeviceKey;
    use kallip_lesche_client::LescheClient;
    use kallip_lesche_common::event::TagmaStatusPayload;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// Captured `POST /upstream` bodies, in arrival order.
    type Capture = Arc<Mutex<Vec<Vec<UpstreamEvent>>>>;
    /// Mock-server shared state: captured batches, the attempt counter,
    /// the failing-attempt selector, and the per-attempt response bodies.
    type MockUpstreamState = (
        Capture,
        Arc<AtomicUsize>,
        Option<usize>,
        Arc<Vec<serde_json::Value>>,
    );

    /// Spawn a mock lesche capturing `POST /upstream` batches. When `fail_nth`
    /// is `Some(n)`, the n-th attempt (0-based) answers 500 instead. Each OK
    /// attempt answers the shipped `"ok"` shape only because the client's
    /// legacy contract response carries no piggyback; see
    /// [`spawn_upstream_lesche_scripted`] for the faces-carrying bodies.
    async fn spawn_upstream_lesche(capture: Capture, fail_nth: Option<usize>) -> String {
        spawn_upstream_lesche_scripted(capture, fail_nth, Vec::new()).await
    }

    /// [`spawn_upstream_lesche`] with per-attempt JSON response bodies: the
    /// n-th OK attempt answers `bodies[n]` (the last entry repeats); an
    /// empty script answers `{"applied": N}` -- no `faces` field, the
    /// legacy shape the reconcile must treat as a no-op.
    async fn spawn_upstream_lesche_scripted(
        capture: Capture,
        fail_nth: Option<usize>,
        bodies: Vec<serde_json::Value>,
    ) -> String {
        let attempts = Arc::new(AtomicUsize::new(0));
        async fn handler(
            axum::extract::State((c, n, fail_nth, bodies)): axum::extract::State<MockUpstreamState>,
            axum::Json(batch): axum::Json<Vec<UpstreamEvent>>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let attempt = n.fetch_add(1, Ordering::SeqCst);
            if fail_nth == Some(attempt) {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
            }
            let body = bodies
                .get(attempt)
                .or_else(|| bodies.last())
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "applied": batch.len() }));
            c.lock().await.push(batch);
            (axum::http::StatusCode::OK, axum::Json(body)).into_response()
        }
        let app = axum::Router::new()
            .route("/tagmata/{tagma}/upstream", axum::routing::post(handler))
            .with_state((capture, attempts, fail_nth, Arc::new(bodies)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn status_payload(consumed: u64) -> TagmaStatusPayload {
        TagmaStatusPayload {
            root_state: AgentState::Idle,
            subagents_total: 0,
            subagents_active: 0,
            token_budget: 1000,
            token_consumed: consumed,
            token_budget_unlimited: false,
        }
    }

    fn projection_payload(push_seq: u64) -> kallip_lesche_common::projection::ProjectionSnapshot {
        serde_json::from_value(serde_json::json!({
            "agents": [],
            "status": {
                "root_state": "idle",
                "subagents_total": 0,
                "subagents_active": 0,
                "token_budget": 1,
                "token_consumed": 0,
            },
            "push_seq": push_seq,
            "work_schedule": null,
        }))
        .expect("snapshot parses")
    }

    /// Build a relay on a real AppState against the mock lesche, with a
    /// shortened flusher retry tick. Returns the handle and the state
    /// strong ref (the flusher holds a Weak).
    async fn setup(capture: Capture, fail_nth: Option<usize>) -> (RelayHandle, SharedState) {
        let state = make_state();
        let root = kallip_common::agentid::AgentId::from("root".to_string());
        let url = spawn_upstream_lesche(capture, fail_nth).await;
        let client = LescheClient::builder(&url, "tok").build().unwrap();
        let handle = RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            root,
            Arc::downgrade(&state),
        );
        handle.inner.flusher_retry_ms.store(60, Ordering::Relaxed);
        (handle, state)
    }

    async fn wait_for_batches(capture: &Capture, n: usize) -> Vec<Vec<UpstreamEvent>> {
        for _ in 0..40 {
            if capture.lock().await.len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        capture.lock().await.clone()
    }

    /// All three plaintext metadata kinds published to the bus reach the
    /// wire as `UpstreamEvent` elements — the full demux surface, end to
    /// end (batching itself is timing-dependent by design).
    #[tokio::test]
    async fn flusher_carries_all_three_variants_to_the_wire() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        // Let the flusher task subscribe before publishing (a no-subscriber
        // publish is a benign drop).
        tokio::time::sleep(Duration::from_millis(100)).await;
        state
            .bus
            .publish(BusStatus(status_payload(12)))
            .expect("status topic");
        state
            .bus
            .publish(BusProjection(projection_payload(7)))
            .expect("projection topic");
        state
            .bus
            .publish(SignalFrame(SignalEvent::Busy))
            .expect("signal topic");
        let batches = wait_for_batches(&capture, 1).await;
        handle.stop_upstream_flusher().await;
        let flat: Vec<&UpstreamEvent> = batches.iter().flatten().collect();
        assert!(flat.len() >= 3, "all three variants reach the wire");
        assert!(
            batches.iter().any(|b| {
                b.iter()
                    .any(|e| matches!(e, UpstreamEvent::Status(p) if p.token_consumed == 12))
            }),
            "status payload arrives verbatim"
        );
        assert!(
            batches.iter().any(|b| {
                b.iter()
                    .any(|e| matches!(e, UpstreamEvent::Projection(s) if s.push_seq == 7))
            }),
            "projection payload arrives verbatim"
        );
        assert!(
            batches.iter().any(|b| b
                .iter()
                .any(|e| matches!(e, UpstreamEvent::Signal(SignalEvent::Busy)))),
            "signal arrives as the Signal variant"
        );
    }

    /// Keep-last retention: a failed flush retains the newest snapshot per
    /// topic and retries on the tick; a signal in the failed batch is NOT
    /// retried (fire-and-forget — the next transition supersedes it).
    #[tokio::test]
    async fn failed_flush_retains_snapshots_but_not_signals() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        // First attempt (status+signal batch) fails; the retry succeeds.
        let (handle, state) = setup(capture.clone(), Some(0)).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        state
            .bus
            .publish(BusStatus(status_payload(5)))
            .expect("status topic");
        state
            .bus
            .publish(SignalFrame(SignalEvent::Busy))
            .expect("signal topic");
        // Wait past the failed attempt + a successful retry tick.
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.stop_upstream_flusher().await;
        let batches = capture.lock().await.clone();
        // The retry carries the retained status...
        assert!(
            batches
                .iter()
                .any(|b| b.iter().any(|e| matches!(e, UpstreamEvent::Status(_)))),
            "retained snapshot is retried to success"
        );
        // ...exactly once (the failed attempt was not captured).
        let status_hits: usize = batches
            .iter()
            .map(|b| {
                b.iter()
                    .filter(|e| matches!(e, UpstreamEvent::Status(_)))
                    .count()
            })
            .sum();
        assert_eq!(status_hits, 1, "keep-last: one successful status flush");
        // ...and the signal is gone: it rode the failed batch, never retained.
        let signal_hits: usize = batches
            .iter()
            .map(|b| {
                b.iter()
                    .filter(|e| matches!(e, UpstreamEvent::Signal(_)))
                    .count()
            })
            .sum();
        assert_eq!(signal_hits, 0, "signals are fire-and-forget, never retried");
    }

    /// Same-topic collapse: two status snapshots published inside one drain
    /// window produce a single `Status` element carrying the NEWER payload —
    /// the dual-producer wire-side convergence (the relay and direct status
    /// drivers both publish).
    #[tokio::test]
    async fn same_topic_duplicates_collapse_to_the_newest() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Back-to-back publishes land in the same drain window (the flusher
        // needs a scheduler hop to wake; both sends complete first).
        state
            .bus
            .publish(BusStatus(status_payload(1)))
            .expect("status topic");
        state
            .bus
            .publish(BusStatus(status_payload(2)))
            .expect("status topic");
        let batches = wait_for_batches(&capture, 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.stop_upstream_flusher().await;
        let statuses: Vec<u64> = batches
            .iter()
            .flatten()
            .filter_map(|e| match e {
                UpstreamEvent::Status(p) => Some(p.token_consumed),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![2], "keep-last: only the newest flushes");
    }

    /// The status payloads of every captured batch, in arrival order.
    async fn status_seq(capture: &Capture) -> Vec<u64> {
        capture
            .lock()
            .await
            .iter()
            .flatten()
            .filter_map(|e| match e {
                UpstreamEvent::Status(p) => Some(p.token_consumed),
                _ => None,
            })
            .collect()
    }

    /// Status gate, default + flip + idempotent re-send:
    /// a fresh relay pushes (the OPEN default), a close suppresses the
    /// wire while the keep-last slot retains, the open edge pushes the
    /// retained snapshot on the wake, and a same-value re-send (the
    /// reconnect `true -> true` re-learn) is a no-op.
    #[tokio::test]
    async fn status_gate_defaults_open_and_owner_sub_flips_it() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Default OPEN: the first publish reaches the wire with no hint.
        state
            .bus
            .publish(BusStatus(status_payload(1)))
            .expect("status topic");
        wait_for_batches(&capture, 1).await;
        assert_eq!(status_seq(&capture).await, vec![1], "default open");
        // Close: subsequent frames retain, nothing flushes.
        handle.handle_status_sub(false).await;
        state
            .bus
            .publish(BusStatus(status_payload(2)))
            .expect("status topic");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(status_seq(&capture).await, vec![1], "gated: no wire");
        // Reopen: the retained full snapshot pushes immediately.
        handle.handle_status_sub(true).await;
        wait_for_batches(&capture, 2).await;
        assert_eq!(
            status_seq(&capture).await,
            vec![1, 2],
            "resume pushes the retained snapshot"
        );
        // Same-value re-send: idempotent, no wake, no POST.
        handle.handle_status_sub(true).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(status_seq(&capture).await, vec![1, 2], "no-op re-send");
        handle.stop_upstream_flusher().await;
    }

    /// In-process immunity, asserted not assumed: the gate lives
    /// only in the flusher's send decision, so the local SSE assembler's
    /// bus subscription keeps receiving every StatusSnapshot while the
    /// wire is gated.
    #[tokio::test]
    async fn gated_wire_keeps_the_local_bus_flowing() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut rx = state.bus.subscribe::<BusStatus>().expect("status topic");
        handle.handle_status_sub(false).await;
        state
            .bus
            .publish(BusStatus(status_payload(9)))
            .expect("status topic");
        let got = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("bus delivery while gated")
            .expect("frame");
        assert_eq!(got.0.token_consumed, 9, "assembler still fed");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(capture.lock().await.is_empty(), "the wire stayed silent");
        handle.stop_upstream_flusher().await;
    }

    /// Keep-last-while-gated never resurrects stale as final state: two
    /// frames retained while gated collapse to the newest on resume, and
    /// a later publish supersedes it on the next flush.
    #[tokio::test]
    async fn resume_pushes_newest_retained_then_supersedes() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.handle_status_sub(false).await;
        state
            .bus
            .publish(BusStatus(status_payload(1)))
            .expect("status topic");
        state
            .bus
            .publish(BusStatus(status_payload(2)))
            .expect("status topic");
        handle.handle_status_sub(true).await;
        wait_for_batches(&capture, 1).await;
        assert_eq!(
            status_seq(&capture).await,
            vec![2],
            "keep-last retains only the newest through the gate"
        );
        state
            .bus
            .publish(BusStatus(status_payload(3)))
            .expect("status topic");
        wait_for_batches(&capture, 2).await;
        assert_eq!(status_seq(&capture).await, vec![2, 3]);
        handle.stop_upstream_flusher().await;
    }

    /// Empty-retain resume: the gate closed before any frame
    /// existed, so the open wake has nothing to push -- the first
    /// delivery is the next published frame (the <= 2 s cadence leg in
    /// production).
    #[tokio::test]
    async fn empty_retain_resume_waits_for_the_next_frame() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.handle_status_sub(false).await;
        handle.handle_status_sub(true).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            capture.lock().await.is_empty(),
            "nothing retained: the wake carries no payload"
        );
        state
            .bus
            .publish(BusStatus(status_payload(4)))
            .expect("status topic");
        wait_for_batches(&capture, 1).await;
        assert_eq!(status_seq(&capture).await, vec![4]);
        handle.stop_upstream_flusher().await;
    }

    /// Cancel-safety: cancelling the flusher while it is parked
    /// in the gate-notify wait exits cleanly -- the slot clears (a fresh
    /// start works against the same capture) and no spurious POST rides
    /// the cancel path.
    #[tokio::test]
    async fn cancel_during_the_gate_wait_exits_cleanly() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let (handle, state) = setup(capture.clone(), None).await;
        handle.start_upstream_flusher().await;
        handle.handle_status_sub(false).await; // gate closed: the loop parks
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.stop_upstream_flusher().await;
        assert!(
            capture.lock().await.is_empty(),
            "cancel must not synthesize a POST"
        );
        handle.start_upstream_flusher().await;
        // Let the fresh loop reach its bus subscriptions before the
        // reopen + publish: a subscribe-after-publish race would miss
        // the frame for reasons unrelated to cancel-safety.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.handle_status_sub(true).await; // reopen: wakes the fresh loop
        state
            .bus
            .publish(BusStatus(status_payload(9)))
            .expect("status topic");
        wait_for_batches(&capture, 1).await;
        assert_eq!(status_seq(&capture).await, vec![9]);
        handle.stop_upstream_flusher().await;
    }

    /// Piggyback reconcile, both directions: a response saying
    /// status:0 closes an open gate; a response saying status:1 opens a
    /// closed one and the retained snapshot pushes; the projection face
    /// reconciles through the same lane.
    #[tokio::test]
    async fn piggyback_counts_flip_the_gate_both_ways() {
        let capture: Capture = Arc::new(Mutex::new(Vec::new()));
        let faces = |s: u64, p: u64| serde_json::json!({"applied": 1, "faces": {"status": s, "projection": p}});
        let url = spawn_upstream_lesche_scripted(
            capture.clone(),
            None,
            vec![faces(0, 0), faces(1, 0), faces(1, 1)],
        )
        .await;
        let state = make_state();
        let root = kallip_common::agentid::AgentId::from("root".to_string());
        let client = LescheClient::builder(&url, "tok").build().unwrap();
        let handle = RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            root,
            Arc::downgrade(&state),
        );
        handle.inner.flusher_retry_ms.store(60, Ordering::Relaxed);
        handle.start_upstream_flusher().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // POST #1 says status:0 -> the open gate closes silently.
        state
            .bus
            .publish(BusStatus(status_payload(1)))
            .expect("status topic");
        wait_for_batches(&capture, 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        state
            .bus
            .publish(BusStatus(status_payload(2)))
            .expect("status topic");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(status_seq(&capture).await, vec![1], "piggyback closed it");
        // POST #2 (a signal rides the same batch lane) says status:1 ->
        // the gate reopens and the retained snapshot pushes immediately.
        state
            .bus
            .publish(SignalFrame(SignalEvent::Busy))
            .expect("signal topic");
        wait_for_batches(&capture, 3).await;
        assert_eq!(
            status_seq(&capture).await,
            vec![1, 2],
            "piggyback reopened; retained frame resumed"
        );
        // The projection face rides the same reconcile lane.
        assert!(
            handle.inner.projection_active.load(Ordering::Relaxed),
            "projection truth flipped through the same handler"
        );
        handle.stop_upstream_flusher().await;
    }
}
