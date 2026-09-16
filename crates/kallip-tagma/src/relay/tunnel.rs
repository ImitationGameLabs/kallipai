//! The lesche tunnel: the SSE stream reader + reconnect loop, and the inbound
//! dispatch fan-out.
//!
//! A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. The pump lifecycle methods it drives (`start/stop_pump`,
//! `start/stop_status_pump`, `start/stop_room_pump`) live in sibling child
//! modules and are already `pub(super)`.

use super::*;

impl RelayHandle {
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
        self.stop_room_pump().await;
        self.stop_projection_pump().await;
        self.stop_upstream_flusher().await;
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
        self.start_room_pump().await;
        self.start_projection_pump().await;
        self.start_upstream_flusher().await;
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
        self.stop_projection_pump().await;
        self.stop_upstream_flusher().await;
        self.stop_room_pump().await;
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
                    error!("relay op dispatch panicked past the inner boundary");
                }
            }
            TunnelInbound::Wake => {
                // A best-effort hint that membership changed: re-poll
                // `list_my_rooms` immediately so the joined-rooms cache warms
                // before the next room envelope arrives (a just-added tagma is
                // blind until this poll lands). The Wake carries no payload, so
                // the sweep re-fetches `list_my_rooms` -- one batched GET,
                // acceptable at the expected volume.
                self.poll_rooms().await;
            }
            TunnelInbound::ManageRest {
                req_id,
                method,
                path,
                body,
                trace,
            } => {
                self.handle_manage_rest(req_id, &path, &method, &trace, body)
                    .await
            }
            TunnelInbound::OwnerSub { face, active } => match face {
                Face::Projection => self.handle_projection_hint(active).await,
                Face::Status => self.handle_status_sub(active).await,
            },
        }
    }
}

/// The frame-surface allowlist: the exact manage_router routes that may
/// ride a ManageRest frame, enumerated one by one. This is the enforcement
/// point for the encryption-scope decision: prompt-bearing routes
/// (/profiles/sets/{name}), the provider probe, session content (the lesche
/// message posts) and destructive deletes stay OFF the plaintext frame.
/// Anything not listed here is a 404, even though the underlying
/// manage_router would happily serve it -- fail-closed by construction.
fn frame_allowed(method: &str, path: &str) -> bool {
    let p = path.trim_end_matches('/');
    let sub = p
        .strip_prefix("/agents/")
        .and_then(|rest| rest.split_once('/'));
    match (method, p) {
        ("GET", "/agents")
        | ("GET", "/budget")
        | ("POST", "/budget")
        | ("GET", "/work-schedule")
        | ("PUT", "/work-schedule")
        | ("GET", "/profiles")
        | ("POST", "/profiles/apply")
        | ("PUT", "/profiles/default") => true,
        // Per-agent subroutes: match the {id} segment explicitly.
        _ => {
            let (m, s) = (method, sub);
            matches!(
                (m, s),
                ("GET", Some((_, "status")))
                    | ("POST", Some((_, "interrupt")))
                    | ("PUT", Some((_, "duty" | "metadata" | "profile-set")))
            )
        }
    }
}

impl RelayHandle {
    /// A ManageRest frame from the lesche reverse-proxy: run the frame-surface
    /// allowlist, then execute against the same manage router the envelope
    /// path uses, replying over the plaintext manage-reply POST (the E2E
    /// emit loop is unreadable to the lesche proxy, which needs the reply
    /// to answer the HTTP caller). Replies are keyed by req_id; trace ids
    /// are only for logging.
    async fn handle_manage_rest(
        &self,
        req_id: u64,
        frame_path: &str,
        method: &str,
        trace: &kallip_archeion_common::ids::TraceId,
        body: serde_json::Value,
    ) {
        if !frame_allowed(method, frame_path) {
            warn!(
                req_id,
                method, frame_path = %frame_path, trace = %trace,
                "manage-rest frame denied by allowlist"
            );
            let reply = ManageRestReply {
                req_id,
                status: 404,
                body: serde_json::json!({"error":{"message":"not on the manage frame surface"}}),
            };
            let _ = self.inner.client.post_manage_reply(&reply).await;
            return;
        }
        debug!(
            req_id, trace = %trace, method, path = %frame_path,
            "manage-rest frame accepted"
        );
        let reply: TagmaReply = AssertUnwindSafe(self.dispatch_manage(method, frame_path, body))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| TagmaReply::ManageResult {
                req_id,
                status: 502,
                body: serde_json::json!({"error":{"message":"relay manage panicked"}}),
            });
        let (status, body) = match &reply {
            TagmaReply::ManageResult { status, body, .. } => (*status, body.clone()),
            _ => (
                502,
                serde_json::json!({"error":{"message":"unexpected reply shape"}}),
            ),
        };
        let out = ManageRestReply {
            req_id,
            status,
            body,
        };
        let _ = self.inner.client.post_manage_reply(&out).await;
    }
}

#[cfg(test)]
mod manage_rest_tests {
    use super::frame_allowed;

    #[test]
    fn allowlist_admits_low_and_medium_sensitive_routes() {
        for (method, path) in [
            ("GET", "/agents"),
            ("GET", "/agents/x/status"),
            ("POST", "/budget"),
            ("GET", "/work-schedule"),
            ("PUT", "/work-schedule"),
            ("GET", "/profiles"),
            ("PUT", "/profiles/default"),
            ("POST", "/profiles/apply"),
            ("GET", "/agents/x/status"),
            ("POST", "/agents/x/interrupt"),
            ("PUT", "/agents/x/duty"),
            ("PUT", "/agents/x/metadata"),
            ("PUT", "/agents/x/profile-set"),
        ] {
            assert!(frame_allowed(method, path), "{method} {path}");
        }
    }

    #[test]
    fn allowlist_denies_prompt_and_unlisted_routes() {
        // Prompt-bearing bodies must never ride the frame surface.
        for (method, path) in [
            ("GET", "/profiles/sets/main"),
            ("PUT", "/profiles/sets/main"),
            ("POST", "/profiles/probe"),
            // Session CONTENT riding the manage router: denied.
            ("POST", "/agents/x/lesche/messages"),
            ("POST", "/agents/x/lesche/rooms/r/messages"),
            ("POST", "/agents/x/lesche/direct-sessions/p/messages"),
            ("GET", "/agents/x/lesche/sessions"),
            ("PUT", "/budget"), // drift: the router serves POST, not PUT
            ("GET", "/agents/x/lesche/direct-sessions/p/messages"), // history read
            ("DELETE", "/profiles/sets/main"),
            ("GET", "/messages"),
            ("DELETE", "/agents/x"),
            ("GET", "/completely/unknown"),
        ] {
            assert!(!frame_allowed(method, path), "{method} {path}");
        }
    }
}

/// Order sensitivity: the call site passes (path, method) into a
/// fn whose parameters are (method, path)-shaped -- swapping them makes
/// every frame hit the deny arm. These two asserts pin the order: the
/// swapped call must NOT be allowed.
#[test]
fn frame_allowed_is_order_sensitive() {
    assert!(frame_allowed("GET", "/agents"));
    assert!(!frame_allowed("/agents", "GET"));
}

/// Frame dispatch end-to-end -- handle_manage_rest
/// against a mock lesche that records the manage-reply POST. Pins the
/// (path, method) call order at the real dispatch site: the correct order
/// must reach the router (200), and a swapped call must deny (404) --
/// the exact regression this pins.
#[cfg(test)]
mod manage_frame_tests {
    use super::*;
    use crate::test_helpers::make_state;
    use std::sync::Arc;

    async fn setup(
        replies: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    ) -> (RelayHandle, crate::state::SharedState) {
        let state = make_state();
        let app = axum::Router::new().route(
            "/tunnel/manage-reply",
            axum::routing::post(move |axum::Json(reply): axum::Json<serde_json::Value>| {
                let replies = replies.clone();
                async move {
                    replies.lock().await.push(reply);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let lesche_url = format!("http://{addr}");
        let client = LescheClient::builder(&lesche_url, "tok").build().unwrap();
        let handle = RelayHandle::new(
            client,
            "test".to_string(),
            TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            e2e::DeviceKey::generate(),
            AgentId::from("root".to_string()),
            Arc::downgrade(&state),
        );
        (handle, state)
    }

    #[tokio::test]
    async fn correct_order_reaches_router_swapped_order_denies() {
        let replies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (handle, state) = setup(replies.clone()).await;
        let _keep_state = &state;
        let trace = kallip_archeion_common::ids::TraceId::random();
        // Correct (path, method) order: the router serves GET /agents.
        handle
            .handle_manage_rest(1, "/agents", "GET", &trace, serde_json::json!({}))
            .await;
        // Swapped order regression: frame_allowed denies.
        handle
            .handle_manage_rest(2, "GET", "/agents", &trace, serde_json::json!({}))
            .await;
        let replies = replies.lock().await;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["status"], 200, "correct order must route");
        assert_eq!(replies[1]["status"], 404, "swapped order must deny");
    }
}
