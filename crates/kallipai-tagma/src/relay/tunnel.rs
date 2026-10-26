//! The keryx tunnel: the SSE stream reader + reconnect loop, and the inbound
//! dispatch fan-out.
//!
//! A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. The pump lifecycle methods it drives (`start/stop_pump`,
//! `start/stop_status_pump`, `start/stop_room_pump`) live in sibling child
//! modules and are already `pub(super)`.

use super::*;

impl RelayHandle {
    /// Hold the keryx tunnel open, reconnecting with a small backoff on any
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

/// The `{name}` segment of `{prefix}/{name}`: a one-segment tail with
/// no further path. The bare prefix and any trailing slash are rejected
/// here, matching the manage router this frame dispatches to: the
/// router normalizes nothing, so a path has to arrive exact.
fn one_segment<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// The frame-surface allowlist: the exact manage_router routes that may
/// ride a ManageRest frame, enumerated one by one. The split is per
/// call, not per family.
///
/// Confidentiality decides most of it: a route rides only when neither
/// its request nor its reply carries provider API keys or lesche
/// message content. The key-bearing save (`PUT /profiles`) and probe
/// (`POST /profiles/probe`) take the envelope; the masked read
/// (`GET /profiles`) and the config writes that carry no key
/// (`apply`, `refresh`, `PUT /profiles/default`) ride here.
///
/// The frame contract decides the rest: the frame carries no query
/// string, so a parameter the body cannot carry keeps a route off it.
/// The set delete is that case -- its `?force=` has nowhere to go, so
/// the call takes the envelope.
///
/// No route under the `/profiles/sets/` prefix is enumerated here, the
/// delete included -- today it is the only route there, and a route
/// added later has to be granted an allow entry of its own before it
/// can land on the plaintext frame.
///
/// The task family has no delete verb at all -- tasks close and
/// archive, so the router serves no `DELETE /tasks/{id}` for the frame
/// to admit.
///
/// The turn family is text: its three reads carry session content, so none
/// of them is enumerated here and they ride the envelope.
///
/// Anything not listed is a 404, even though the underlying
/// manage_router would happily serve it -- fail-closed by construction.
fn frame_allowed(method: &str, path: &str) -> bool {
    // The frame admits exact shapes: an empty id is not an id, so
    // `/agents//duty` -- which splits into ("", "duty") -- stays off the
    // `duty` arm below.
    let sub = path
        .strip_prefix("/agents/")
        .and_then(|rest| rest.split_once('/'))
        .filter(|(id, _)| !id.is_empty());
    // A destructive verb is no bar by itself: the manage frame carries
    // the operator identity for every request it dispatches, and a bare
    // /agents/{id} takes no parameter and carries no secret -- a 204
    // with an empty body -- so removal rides the reads' criterion.
    let agent_detail = one_segment(path, "/agents/");
    let task_detail = one_segment(path, "/tasks/");
    match (method, path) {
        ("GET", "/agents")
        | ("GET", "/budget")
        | ("POST", "/budget")
        | ("GET", "/work-schedule")
        | ("PUT", "/work-schedule")
        | ("GET", "/profiles")
        | ("POST", "/profiles/apply")
        | ("POST", "/profiles/refresh")
        | ("PUT", "/profiles/default") => true,
        ("GET", "/tasks") => true,
        // Task detail: GET /tasks/{id} (two segments, read-only; the
        // static /tasks/export route is absorbed by the same shape and
        // is equally read-only).
        _ => {
            // Per-agent subroutes: match the {id} segment explicitly.
            let (m, s) = (method, sub);
            matches!(
                (m, s),
                ("GET", Some((_, "status")))
                    | ("POST", Some((_, "interrupt")))
                    | ("PUT", Some((_, "duty" | "metadata" | "profile-set")))
            ) || matches!((method, agent_detail), ("DELETE", Some(_)))
                || matches!((method, task_detail), ("GET", Some(_)))
        }
    }
}

impl RelayHandle {
    /// A ManageRest frame from the keryx reverse-proxy: run the frame-surface
    /// allowlist, then execute against the same manage router the envelope
    /// path uses, replying over the plaintext manage-reply POST (the E2E
    /// emit loop is unreadable to the keryx proxy, which needs the reply
    /// to answer the HTTP caller). Replies are keyed by req_id; trace ids
    /// are only for logging.
    async fn handle_manage_rest(
        &self,
        req_id: u64,
        frame_path: &str,
        method: &str,
        trace: &kallipai_archeion_common::ids::TraceId,
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
        let reply: TagmaReply =
            AssertUnwindSafe(self.dispatch_manage(method, frame_path, body, None))
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
    fn allowlist_admits_the_listed_routes() {
        for (method, path) in [
            ("GET", "/agents"),
            ("GET", "/agents/x/status"),
            ("POST", "/budget"),
            ("GET", "/work-schedule"),
            ("PUT", "/work-schedule"),
            ("GET", "/profiles"),
            ("PUT", "/profiles/default"),
            ("POST", "/profiles/apply"),
            ("POST", "/profiles/refresh"),
            ("GET", "/tasks"),
            ("GET", "/tasks/7"),
            ("GET", "/tasks/export"),
            ("GET", "/agents/x/status"),
            ("POST", "/agents/x/interrupt"),
            ("PUT", "/agents/x/duty"),
            ("PUT", "/agents/x/metadata"),
            ("PUT", "/agents/x/profile-set"),
            ("DELETE", "/agents/x"),
        ] {
            assert!(frame_allowed(method, path), "{method} {path}");
        }
    }

    #[test]
    fn allowlist_denies_confidential_and_unlisted_routes() {
        // Key-bearing bodies and session content never ride the frame.
        for (method, path) in [
            ("GET", "/profiles/sets/main"),
            ("PUT", "/profiles/sets/main"),
            ("POST", "/profiles/probe"),
            ("PUT", "/profiles"), // the key-bearing save
            // Session CONTENT riding the manage router: denied.
            ("POST", "/agents/x/lesche/messages"),
            ("POST", "/agents/x/lesche/rooms/r/messages"),
            ("POST", "/agents/x/lesche/direct-sessions/p/messages"),
            ("GET", "/agents/x/lesche/sessions"),
            ("PUT", "/budget"), // drift: the router serves POST, not PUT
            ("GET", "/agents/x/lesche/direct-sessions/p/messages"), // history read
            ("DELETE", "/profiles/sets/main"),
            ("GET", "/messages"),
            ("GET", "/completely/unknown"),
            // The turn history carries conversation text, so its reads
            // take the E2E envelope like every other content read.
            ("GET", "/agents/x/turns"),
            ("GET", "/agents/x/turns/7"),
            ("GET", "/agents/x/turns/7/calls/0/body"),
            // The single-segment filter is the removal arm's safety
            // basis: a read on that path, a multi-segment tail, an empty
            // id and a lower-case method all stay off. A trailing slash
            // stays off this frame, and the router it dispatches to
            // matches exact shapes too: relay::manage's
            // `manage_router_trailing_slash_is_404` pins the router's own
            // refusal of that spelling. The same shape rule denies the
            // empty-id spelling of an admitted subroute.
            ("PUT", "/agents//duty"),
            ("POST", "/agents//interrupt"),
            ("GET", "/agents/x"),
            ("DELETE", "/agents/x/duty"),
            // The read cursor is partition-scoped and a plaintext frame
            // carries no peer, so admitting it would write the operator
            // partition: only the E2E envelope path may call it.
            ("PUT", "/agents/x/external/read-cursor"),
            ("DELETE", "/agents/x/sub"),
            ("DELETE", "/agents/"),
            ("DELETE", "/agents//x"),
            ("delete", "/agents/x"),
            ("DELETE", "/agents/x/"),
            ("GET", "/agents/"),
            // Task writes stay off the frame: the POSTs are
            // three-segment subroutes the read-only arm never admits,
            // and the family has no delete verb to admit -- tasks
            // close and archive, so the router serves no DELETE at all.
            ("POST", "/tasks/7/start"),
            ("POST", "/tasks/7/close"),
            ("DELETE", "/tasks/7"),
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
/// against a mock keryx that records the manage-reply POST. Pins the
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
        let keryx_url = format!("http://{addr}");
        let client = LescheClient::builder(&keryx_url, "tok").build().unwrap();
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
        let trace = kallipai_archeion_common::ids::TraceId::random();
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

    /// The refresh frame regression: the profiles refetch button rides
    /// the remote manage surface. The default test state runs a local
    /// source, so the router's own refusal (409) is the pass-through
    /// proof -- 404 would mean the allowlist denied the frame.
    #[tokio::test]
    async fn refresh_frame_reaches_router() {
        let replies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (handle, state) = setup(replies.clone()).await;
        let _keep_state = &state;
        let trace = kallipai_archeion_common::ids::TraceId::random();
        handle
            .handle_manage_rest(
                1,
                "/profiles/refresh",
                "POST",
                &trace,
                serde_json::json!({}),
            )
            .await;
        let replies = replies.lock().await;
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0]["status"], 409,
            "refresh must clear the frame allowlist"
        );
    }

    /// Removing an agent rides the plaintext frame. The registry has no such
    /// agent, so the router answers with its own 404 -- reaching that answer
    /// (rather than the frame-surface refusal) is the pass-through proof.
    #[tokio::test]
    async fn remove_agent_frame_reaches_router() {
        let replies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (handle, state) = setup(replies.clone()).await;
        let _keep_state = &state;
        let trace = kallipai_archeion_common::ids::TraceId::random();
        handle
            .handle_manage_rest(1, "/agents/ghost", "DELETE", &trace, serde_json::json!({}))
            .await;
        let replies = replies.lock().await;
        assert_eq!(replies.len(), 1);
        // 404 alone would not discriminate -- a frame refusal replies 404
        // too -- so the body is the proof: this is the router's own answer.
        assert_eq!(replies[0]["status"], 404);
        assert_eq!(
            replies[0]["body"]["error"]["message"], "agent not found",
            "the DELETE must clear the frame allowlist"
        );
    }
}
