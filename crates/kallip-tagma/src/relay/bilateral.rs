//! The bilateral (1:1) path: KEX handling, inbound envelope decryption + op
//! routing, and the encrypted/plaintext emit tails.
//!
//! Extracted from `mod.rs`. A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. Every method here is `pub(super)`: `handle_kex` and
//! `handle_user_op` are driven by `tunnel::dispatch`; `emit`/`emit_signal` are
//! driven by `dispatch` and by the pump (`pump.rs`).

use super::*;

impl RelayHandle {
    /// Respond to a key exchange. This is also the re-KEX boundary: cancel any
    /// running pump, install the new key with reset counters, then (re)start the
    /// pump. Cancelling the pump first guarantees no emit using the old key is
    /// in flight when the counter resets, so re-KEX cannot reuse a nonce.
    pub(super) async fn handle_kex(
        &self,
        conversation_id: ConversationId,
        init: kallip_lesche_common::control::KeyExchangeInit,
    ) {
        let (response, key) = match kex::respond_key_exchange(
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
        // `first` distinguishes the initial KEX from a re-KEX (the cadence the
        // user sees in the logs); a re-KEX means a prior key was already installed.
        let first = {
            let mut c = self.inner.crypto.lock().await;
            let had_key = c.key.is_some();
            c.key = Some(key);
            c.outbound_seq = 0;
            c.seen_inbound = None;
            !had_key
        };
        self.start_pump().await;
        if let Err(e) = self
            .inner
            .client
            .post_key_exchange_response(&conversation_id, &response)
            .await
        {
            warn!(conv = %conversation_id, "post key-exchange response: {e:#}");
        }
        // `conv` is the relay-plane key (what agora/lesche index this KEX by),
        // so it carries the cross-service correlation value; the tagma id is a
        // constant 1:1 derivation of it and adds nothing per-line.
        info!(conv = %conversation_id, first, "relay KEX completed");
        // History is pull-based: the app sends a `TagmaControl::History` request
        // once it has hydrated its local cache.
    }

    /// Decrypt an app-originated envelope and dispatch it: a `TagmaRequest`
    /// drives the root agent (and emits its reply); a `TagmaControl` is a
    /// sync/plumbing op (today: history pull). The agent-driving branch runs
    /// under a req_id-aware panic boundary so a bug never leaves the app
    /// hanging: a panic yields an `Error` reply for the exact `req_id`.
    ///
    /// `req_id` is peeled from the decoded JSON *before* the branch so the
    /// outer `catch_unwind` (in [`Self::dispatch`]) can attribute a panic that
    /// occurs anywhere downstream — both `TagmaRequest` and `TagmaControl`
    /// carry a top-level `req_id`.
    pub(super) async fn handle_user_op(&self, envelope: Envelope) {
        // The relay-authenticated sender (id + advisory handle). Thread it down
        // to the dispatch tails.
        let sender = envelope.sender.clone();
        let Some(state) = self.inner.state.upgrade() else {
            // The AppState is gone (shutdown/teardown); the envelope is dropped
            // silently by design. Logged at debug so a drain-time drop stays
            // diagnosable without spamming healthy shutdowns.
            debug!(conv = %envelope.conversation_id, "user op dropped: relay shutting down");
            return;
        };
        let room = RoomId::from(envelope.conversation_id.as_ref().to_string());
        // A 1:1 conversation_id is a v5 UUID (ConversationId::for_tagma) and a
        // room_id is a v4 UUID, so the two string spaces are disjoint -- a 1:1
        // envelope can never collide with a joined room, and falls through to
        // the bilateral path below.
        //
        // Room path: a joined room's envelope payload IS the plaintext
        // `RoomMessage` JSON bytes (rooms are plaintext server-readable; the
        // lesche data plane stores room payloads opaquely and enforces member
        // access). It dispatches through its OWN tail (room-annotated prompt,
        // no bilateral ACK). Anything else is the bilateral-KEX 1:1 path, which
        // stays user-only. Checked before the bilateral `kind != Human` filter
        // so an Agent sender in a room is not silently dropped.
        if state.joined_rooms.is_joined(&room).await {
            let payload = RoomPayload {
                // The relay-authenticated sender id (the lesche validates id +
                // kind against the authed principal before stamping the row).
                sender_id: sender.id.as_ref().to_string(),
                plaintext: envelope.ciphertext.0.clone(),
            };
            self.handle_room_message(&state, &room, sender, payload)
                .await;
            return;
        }
        // Bilateral 1:1 KEX path. Only user->tagma envelopes drive ops.
        if envelope.sender.kind != ParticipantKind::Human {
            return;
        }
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
        drop(c);
        // Parse once to a Value, then dispatch by the `op` discriminant. The
        // `op` tags of `TagmaRequest` (send_message/interrupt) and
        // `TagmaControl` (history) are disjoint (pinned by
        // `request_and_control_op_tags_are_disjoint`), so this routing is total.
        let value: serde_json::Value = match serde_json::from_slice(&plain) {
            Ok(v) => v,
            Err(e) => {
                warn!("op decode failed: {e}");
                return;
            }
        };
        let req_id = value.get("req_id").and_then(|v| v.as_u64()).unwrap_or(0);
        let trace = op_trace(req_id);
        let op = value.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "send_message" | "interrupt" => {
                let request: TagmaRequest = match serde_json::from_value(value) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(req_id, "request decode failed: {e}");
                        return;
                    }
                };
                self.handle_agent_op(&trace, req_id, sender.clone(), request)
                    .await;
            }
            "history" => {
                let ctrl: TagmaControl = match serde_json::from_value(value) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(req_id, "control decode failed: {e}");
                        return;
                    }
                };
                let TagmaControl::History {
                    after,
                    before,
                    limit,
                    ..
                } = ctrl;
                self.handle_history(&trace, req_id, &sender, after, before, limit)
                    .await;
            }
            other => warn!(req_id, op = other, "unknown op; dropping"),
        }
    }

    /// Push a runtime signal (busy/idle presence, turn terminals, errors) to
    /// the relay for plaintext rebroadcast as a `LescheEvent::TagmaSignal`.
    /// Signals are operator metadata: they do NOT enter the encrypted envelope
    /// channel and are NOT persisted in `chat_history` (a reconnect only
    /// replays authored messages). Best-effort and not retried, mirroring
    /// [`post_status`](kallip_lesche_client::LescheClient::post_status): a
    /// signal is a transient transition that the next event supersedes, so a
    /// dropped POST just means the UI misses one frame of presence, and the
    /// projector has already logged it for observability. `cancel` aborts an
    /// in-flight POST on re-KEX/shutdown.
    pub(super) async fn emit_signal(
        &self,
        event: kallip_common::protocol::SignalEvent,
        cancel: Option<&CancellationToken>,
    ) -> Result<()> {
        let post = self.inner.client.post_signal(&self.inner.tagma_id, &event);
        match cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => Ok(()),
                r = post => r,
            },
            None => post.await,
        }
    }

    /// Encrypt `reply` for the conversation and post an envelope attributed to
    /// `sender`. Returns `Err` when delivery is exhausted (the pump logs and
    /// carries on; app recovery is via host-history re-pull on reconnect).
    /// `cancel`, when set (the pump path), aborts an in-flight POST on
    /// re-KEX/shutdown instead of waiting out its 30 s timeout.
    ///
    /// `sender` is stamped onto the envelope: the agent for outbound content
    /// and op-replies, the user for the inbound `UserMessage` echo and for
    /// replayed user rows, so a replayed user row is attributed to its sender,
    /// not the tagma.
    pub(super) async fn emit(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        sender: Participant,
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
                sender,
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
}
