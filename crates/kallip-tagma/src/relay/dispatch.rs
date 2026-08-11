//! Inbound op dispatch: routing a decrypted envelope to the root agent, running
//! the op, emitting the reply, replaying history batches, and the root-agent
//! interrupt (its sole caller is `execute_op`, in this module).
//!
//! Extracted from `mod.rs`. A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. The entry points (`handle_room_message`, `handle_agent_op`,
//! `handle_history`) are `pub(super)` because the bilateral path
//! (`bilateral::handle_user_op`) calls into them; `handle_history_inner`,
//! `execute_op`, and `interrupt_root` stay private (same-module callers only).
//! The encrypted/plaintext emit tails (`emit`, `emit_signal`) live in
//! `bilateral` and are likewise `pub(super)`.

use super::*;

impl RelayHandle {
    /// Dispatch a plaintext room message to the root agent -- the room path's
    /// own tail (separate from the bilateral `handle_agent_op`). Parses the
    /// payload as a `RoomMessage` (rooms and the bilateral 1:1 path are disjoint
    /// address spaces; a room message is just text). Enqueues a room-annotated
    /// prompt (`[From: ... | room <id>]`) so the agent can reply with `kallip
    /// lesche send --room <room>`. Skips the bilateral `record_inbound` (rooms
    /// bypass the projector) and the bilateral `MessageAccepted` ACK emit
    /// (lesche's synchronous 202 is the room ACK).
    pub(super) async fn handle_room_message(
        &self,
        state: &SharedState,
        room: &RoomId,
        sender: Participant,
        payload: RoomPayload,
    ) {
        let request: RoomMessage = match serde_json::from_slice(&payload.plaintext) {
            Ok(r) => r,
            Err(e) => {
                warn!(room = %room, "room message decode failed: {e}");
                return;
            }
        };
        let RoomMessage { text } = request;
        // The sender identity is non-forgeable: it is the relay-authenticated
        // `envelope.sender.id` (the lesche validates id + kind against the
        // authed principal before stamping the row), decoded here as the
        // authoritative derived participant id string -- `ParticipantId::
        // for_tagma(...)` on the tagma side, `ParticipantId::for_user(...)` on
        // the user side. Never read the sender from payload bytes.
        let sender_id = payload.sender_id;
        // The advisory `Participant` carries the kind + handle. The kind is
        // relay-authenticated transitively (a user credential cannot post an
        // `Agent` sender, and vice versa, or the lesche's require_* check
        // rejects it); the handle is spoofable + sanitized in `format_room_incoming`.
        let sender_kind = sender.kind.as_str();
        let sender_handle = sender.handle.clone();
        if let Err(e) = crate::routes::deliver_inbound_room_message(
            state,
            &self.inner.root_agent,
            room,
            sender_kind,
            &sender_id,
            sender_handle,
            &text,
        )
        .await
        {
            warn!(room = %room, "room message deliver failed: {e:#}");
        }
    }

    /// Run a `TagmaRequest` against the root agent and emit the reply, under a
    /// req_id-aware panic boundary. The inbound `SendMessage` is persisted by
    /// the external projector inside `deliver_message` (the shared seam
    /// `execute_op` calls), which also publishes the stamped `UserMessage` frame
    /// the pump forwards — so the app dedups its optimistic user line off that
    /// frame, not this ack. The `MessageAccepted` reply therefore carries
    /// `history_id = 0`. `Interrupt` carries no content and is not stored — its
    /// visible effect is the agent's outbound `Interrupted` event, which the
    /// pump already forwards.
    pub(super) async fn handle_agent_op(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        sender: Participant,
        request: TagmaRequest,
    ) {
        let result = AssertUnwindSafe(self.execute_op(sender, request))
            .catch_unwind()
            .await;
        let reply = match result {
            Ok(Ok(reply)) => reply,
            Ok(Err(e)) => {
                warn!(req_id, "op failed: {e:#}");
                op_err_reply(req_id, &e)
            }
            Err(_) => {
                error!(req_id, "op panicked; emitting 502");
                TagmaReply::Error {
                    req_id,
                    status: 502,
                    message: "relay op panicked".to_string(),
                }
            }
        };
        // Op replies pass `cancel: None`: an in-flight reply POST is left to
        // complete across a re-KEX rather than aborted. This is safe by key
        // rotation (a re-KEX swaps the AEAD key, so an old-key envelope the app
        // receives post-rotation is simply undecryptable and dropped), and
        // aborting would discard a user-visible ack. The pump path, by contrast,
        // passes its cancel token so a slow event emit cannot stall a re-KEX.
        if let Err(e) = self.emit(trace, self.agent_sender(), reply, None).await {
            warn!(req_id, "emit reply: {e:#}");
        }
    }

    /// Hard cap on the number of rows one `History` batch returns, regardless
    /// of the `limit` the app requests. Bounds the per-batch POST time (each
    /// row is a separate encrypted POST) so a misbehaving client cannot stall
    /// delivery. The app paginates with `after`/`before` for more.
    const HISTORY_BATCH_MAX: u32 = 50;

    /// Respond to a `TagmaControl::History` request: read the matching window
    /// from `chat_history` (no crypto lock), re-encrypt each row under the
    /// current epoch key, POST it, then POST a `HistoryBatchEnd` marker. The
    /// marker is the *sole* completion signal — if any POST in the batch fails
    /// (503 exhausted), the loop stops WITHOUT the marker, and the app times
    /// out and retries on the next reconnect (its cursor has not advanced past
    /// the un-delivered rows). Lock discipline mirrors [`Self::emit`]: SQLite
    /// reads run outside the crypto lock; each `emit` locks only for CPU
    /// encrypt and releases before its POST, so a slow `post_envelope` cannot
    /// stall live delivery.
    ///
    /// `more` is true when a paginated (`after`/`before`) query returned exactly
    /// `limit` rows (more may remain pullable); the `latest` snapshot mode is
    /// always `more=false`. `count` is the rows actually emitted (decode
    /// failures are skipped, not counted).
    pub(super) async fn handle_history(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        peer: &Participant,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        // req_id-aware panic boundary (mirrors `handle_agent_op`): a panic in
        // the batch is contained and surfaces as an empty `HistoryBatchEnd` so
        // the app is not left waiting on its deadline; it retries on reconnect.
        if AssertUnwindSafe(self.handle_history_inner(trace, req_id, peer, after, before, limit))
            .catch_unwind()
            .await
            .is_err()
        {
            error!(req_id, "history batch panicked; emitting empty marker");
            if let Err(e) = self
                .emit(
                    trace,
                    self.agent_sender(),
                    TagmaReply::HistoryBatchEnd {
                        req_id,
                        count: 0,
                        more: false,
                    },
                    None,
                )
                .await
            {
                warn!(req_id, "history panic-marker emit failed: {e:#}");
            }
        }
    }

    async fn handle_history_inner(
        &self,
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        peer: &Participant,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        let limit = limit.min(Self::HISTORY_BATCH_MAX);
        // Read + decode through the external projector, the single history read
        // path shared with the direct `/external/history` endpoint. Returns
        // decoded replies + the `more` flag. Filter to THIS peer's partition so
        // each device sees only its own conversation.
        let Some(projector) = self
            .inner
            .state
            .upgrade()
            .and_then(|s| s.external.get().cloned())
        else {
            // Tagma shutting down: nothing to emit.
            return;
        };
        let (entries, more) = projector
            .read_history(Some(peer.id.as_ref()), after, before, limit)
            .await;
        let mut count = 0u32;
        for entry in entries {
            // Stamp the row's stored sender (user for an inbound row, agent for
            // outbound) — NOT a hard-coded agent — so a replayed user row arrives
            // in an envelope correctly attributed to the user.
            if let Err(e) = self.emit(trace, entry.sender, entry.reply, None).await {
                warn!(
                    req_id,
                    "history emit failed: {e:#}; stopping batch (no marker)"
                );
                // Deliberately do NOT emit HistoryBatchEnd: partial delivery
                // means the app's cursor has not advanced past the un-delivered
                // rows, and the next reconnect re-requests from `maxRendered`.
                return;
            }
            count += 1;
        }
        if let Err(e) = self
            .emit(
                trace,
                self.agent_sender(),
                TagmaReply::HistoryBatchEnd {
                    req_id,
                    count,
                    more,
                },
                None,
            )
            .await
        {
            warn!(req_id, "history batch-end emit failed: {e:#}");
        }
    }

    /// Translate one op into an in-process tagma call against the root agent and
    /// produce the matching reply. (The former standalone connector did this
    /// over HTTP via `TagmaClient`; the fold calls the registry directly.)
    ///
    /// This runs under the `catch_unwind` boundary in `handle_agent_op`, so it
    /// must stay unwind-safe: no non-`UnwindSafe` guard (e.g. a `MutexGuard`)
    /// may be held across a `.await`. `deliver_message`/`interrupt_root` scope
    /// their registry guards to synchronous blocks and release them before any
    /// await — keep it that way if you extend them.
    async fn execute_op(&self, sender: Participant, request: TagmaRequest) -> Result<TagmaReply> {
        let state = self
            .inner
            .state
            .upgrade()
            .context("tagma shutting down; relay op dropped")?;
        match request {
            TagmaRequest::SendMessage { req_id, text } => {
                let resp = crate::routes::deliver_message(
                    &state,
                    Identity::Operator,
                    Some(sender),
                    &self.inner.root_agent,
                    &text,
                )
                .await?;
                Ok(TagmaReply::MessageAccepted {
                    req_id,
                    queue_depth: resp.queue_depth,
                    warning: resp.warning,
                    // Stamped by `handle_agent_op` with the inbound row id; 0
                    // until then (and when no history store is configured).
                    history_id: 0,
                    // Stamped by `handle_agent_op` with the inbound row's
                    // created_at; absent until then.
                    created_at: None,
                })
            }
            TagmaRequest::Interrupt { req_id } => {
                self.interrupt_root(&state).await?;
                Ok(TagmaReply::Interrupted { req_id })
            }
        }
    }

    /// Cancel the root agent's current round (interrupt), mirroring the
    /// `interrupt_agent` route in-process. No auth needed — the relay is the
    /// trusted operator-equivalent.
    async fn interrupt_root(&self, state: &SharedState) -> Result<()> {
        let round_cancel = {
            let registry = state.registry.read().await;
            let Some(entry) = registry.get(&self.inner.root_agent) else {
                anyhow::bail!("root agent not found for interrupt");
            };
            let live = entry
                .as_live()
                .context("root agent is faulted; nothing to interrupt")?;
            live.agent.round_cancel.clone()
        };
        if let Some(round) = round_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            round.cancel();
        }
        Ok(())
    }
}
