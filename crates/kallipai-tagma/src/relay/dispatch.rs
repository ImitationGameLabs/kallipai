//! Inbound op dispatch: routing a decrypted envelope to the root agent, running
//! the op, emitting the reply, replaying history batches, and the root-agent
//! interrupt (its sole caller is `execute_op`, in this module).
//!
//! A child module of `relay`, so `use super::*` reuses
//! the parent's private imports and grants access to [`RelayHandle`]'s private
//! fields/methods. The entry points (`handle_relay_message`, `handle_agent_op`,
//! `handle_history`) are `pub(super)` because the bilateral path
//! (`bilateral::handle_user_op`) calls into them; `handle_history_inner`,
//! `execute_op`, and `interrupt_root` stay private (same-module callers only).
//! The encrypted emit tail (`emit`) lives in `bilateral` and is likewise
//! `pub(super)`; the plaintext signal uplink rides the upstream flusher.

use kallipai_lesche_common::rooms::MemberId;

use crate::delivery::QUOTE_READ_TIMEOUT;
use crate::messaging::QuoteRead;

use super::*;

// Why a one-row read produced no row. Each case is stated in the log on its
// own: which one it was is what tells a fetch that failed apart from a page
// that landed beside the position, or a row whose payload no longer decodes.
const QUOTE_READ_FETCH_FAILED: &str = "the page fetch failed";
const QUOTE_READ_ROW_MISSING: &str = "the fetched page did not hold that row";
const QUOTE_READ_DECODE_FAILED: &str = "the row did not decode";
const QUOTE_READ_POSITION_OUT_OF_RANGE: &str = "the position leaves no page to fetch";
const QUOTE_READ_REFUSED: &str = "the surface would not show that row";

/// A read that produced no row to render, in the two shapes the reference tells
/// apart. An absence means the read answered and its answer is that the log
/// holds no row there: the page came back without that row, the position leaves
/// no page to fetch, or the 1:1 store holds no such row. A failure means the
/// read did not hand the row back -- the fetch failed, the payload did not
/// decode, the budget ran out, or the surface answered for a log it will not
/// show this reader -- so the row may well be there while its body never
/// arrived. The reason rides along for the log alone: the rendered reference
/// states the shape, never the reason.
enum NoRow {
    Absent(&'static str),
    Failed(&'static str),
}

/// A failed fetch, which is always a failure and never an absence: a lesche
/// answering 404 says no more than that it will not show this reader the row it
/// asked for (an unknown room or session, or a reader who is not a member), so
/// that answer does not establish that the log holds no such row.
fn no_row_of_fetch(err: &anyhow::Error) -> NoRow {
    match err.downcast_ref::<kallipai_lesche_client::LescheHttpError>() {
        Some(http) if http.status.as_u16() == 404 => NoRow::Failed(QUOTE_READ_REFUSED),
        _ => NoRow::Failed(QUOTE_READ_FETCH_FAILED),
    }
}

/// The reference a fetched row renders as: that row's relay-authenticated
/// sender and its text. Both surfaces hand back those same two facts, so
/// the view is built here once instead of in each fetch branch.
fn quote_view_of(
    sender: &kallipai_lesche_common::message::Participant,
    text: String,
) -> crate::messaging::QuoteView {
    crate::messaging::QuoteView {
        author: crate::messaging::relay_author(
            sender.kind.as_str(),
            sender.id.as_ref(),
            &sender.handle,
        ),
        text,
    }
}

impl RelayHandle {
    /// Dispatch a plaintext relay-surface message (room or direct session) to
    /// the root agent -- the relay path's own tail (separate from the bilateral
    /// `handle_agent_op`). Parses the payload per surface (`RoomMessage` vs
    /// `DirectMessage`; both relay surfaces are plaintext, disjoint from the
    /// bilateral 1:1 address space) and enqueues a surface-annotated prompt
    /// (`[From: ... | room <id>]` / `[From: ... | direct <id>]`) so the agent
    /// can reply with `kallip lesche send --room <room>` / `send --tagma
    /// <tagma-id>`. Skips the bilateral `record_inbound` (relay surfaces
    /// bypass the projector) and the bilateral `MessageAccepted` ACK emit
    /// (lesche's synchronous 202 is the ACK).
    pub(super) async fn handle_relay_message(
        &self,
        state: &SharedState,
        surface: crate::messaging::Surface<'_>,
        sender: Participant,
        payload: RoomPayload,
    ) {
        // Each surface decodes its own payload type (`RoomMessage` /
        // `DirectMessage`) rather than one shared type, so a field that belongs
        // to one surface cannot silently appear in the other's decode.
        // The attachment field is deliberately dropped at this decode:
        // attachments do not render on the relay surfaces (direct
        // included).
        //
        // Both payloads also carry `quote`: the position, in that surface's
        // own log, that this message answers -- a room's `seq`, or the seq of
        // the row it answers in the session's own transcript. It is read here
        // because the decode is the one place the row is still in hand, and it
        // travels with the message into the delivery seam. An absent field is
        // no quote, which is the fact, not a placeholder.
        //
        // Mentions are read the same way and for the same reason: the room
        // payload names the members it addresses, the direct payload has no
        // such field. They are not judged or routed on here -- the header the
        // recipient reads is their only consumer on this side.
        let decoded: Result<(String, Option<i64>, Vec<MemberId>), _> = match surface {
            crate::messaging::Surface::Room(_) => {
                serde_json::from_slice::<RoomMessage>(&payload.plaintext)
                    .map(|r| (r.text, r.quote, r.mentions))
            }
            crate::messaging::Surface::Direct(_) => {
                serde_json::from_slice::<DirectMessage>(&payload.plaintext)
                    .map(|r| (r.text, r.quote, Vec::new()))
            }
        };
        let (text, quote, mentions) = match decoded {
            Ok(decoded) => decoded,
            Err(e) => {
                warn!(
                    surface = surface.label(),
                    "relay message decode failed: {e}"
                );
                return;
            }
        };
        // Mentions are read as this tagma's own view because this tagma is the
        // recipient: the header says whether the message names it and how many
        // other members are named, never what the room's roster holds. A
        // surface without a member list decodes as an empty list, which has no
        // view at all.
        let mentions =
            crate::messaging::MentionView::of(&mentions, &MemberId::for_tagma(self.tagma_id()));
        // The sender identity is non-forgeable: it is the relay-authenticated
        // `envelope.sender.id` (the keryx validates id + kind against the
        // authed principal before stamping the row), decoded here as the
        // authoritative derived participant id string -- `ParticipantId::
        // for_tagma(...)` on the tagma side, `ParticipantId::for_user(...)` on
        // the user side. Never read the sender from payload bytes.
        let sender_id = payload.sender_id;
        // The advisory `Participant` carries the kind + handle. The kind is
        // relay-authenticated transitively (a user credential cannot post an
        // `Agent` sender, and vice versa, or the keryx's require_* check
        // rejects it); the handle is spoofable + sanitized in
        // `format_relay_incoming`.
        // Read back the row this message answers, from the same surface history
        // the receiver would read: the reference the agent gets is what that
        // log holds, not what the payload claims to answer. A missing or slow
        // read degrades the reference line alone -- the message is delivered
        // either way.
        let quote_ref = match quote.filter(|position| *position > 0) {
            Some(position) => Some(crate::messaging::QuoteRef {
                position,
                read: self.read_quoted_row(surface, position).await,
            }),
            None => None,
        };
        let inbound = crate::messaging::RelayInbound {
            surface,
            sender_kind: sender.kind.as_str(),
            sender_id: &sender_id,
            sender_handle: &sender.handle,
            text: &text,
            quote: quote_ref,
            mentions,
        };
        if let Err(e) =
            crate::delivery::deliver_inbound_relay_message(state, &self.inner.root_agent, &inbound)
                .await
        {
            warn!(
                surface = surface.label(),
                "relay message deliver failed: {e:#}"
            );
        }
    }

    /// The row a relay-surface message answers, read back from that surface's
    /// own history: one fetch, then a check that the answer is the row asked
    /// for (a page that landed on the neighbouring row is not that row).
    ///
    /// Both surfaces store their log in the lesche, so this is the only read
    /// available, and it is bounded by [`QUOTE_READ_TIMEOUT`] as a whole: the
    /// reference is an aid and the message is the cargo, so a slow relay costs
    /// the reference line, never the delivery.
    ///
    /// A read that produces no row logs why, with the surface and the position
    /// -- the same treatment the bilateral read gives its own failures, so a
    /// reference that cannot be rendered never vanishes without a trace.
    ///
    /// The two no-row shapes stay apart. A log that holds no row at the
    /// position -- a page beside it, or a position with no page to fetch -- is
    /// [`QuoteRead::Absent`]. A fetch that broke, a surface refusing to show
    /// this reader the row (its 404), a payload that no longer decodes, or a
    /// read past the budget is [`QuoteRead::Failed`]. The first says the
    /// history is missing that row; the second says nothing about the row at
    /// all.
    ///
    /// Both surfaces number rows from 1 and callers pass only positive
    /// positions; each branch still states what it does with a position that
    /// leaves no page to fetch (a room's page would run past what the log
    /// holds, a direct log has no page below its first row) rather than
    /// trusting the caller to have held that precondition.
    async fn read_quoted_row(
        &self,
        surface: crate::messaging::Surface<'_>,
        position: i64,
    ) -> QuoteRead {
        let client = self.lesche_client();
        let read = async move {
            match surface {
                crate::messaging::Surface::Room(room) => {
                    // A room's one-row lookup is the page ending below the next
                    // position, clamped to one row.
                    let rows = client
                        .fetch_room_messages(
                            room,
                            kallipai_lesche_client::RoomHistoryQuery {
                                after_seq: None,
                                before_seq: Some(
                                    position
                                        .checked_add(1)
                                        .ok_or(NoRow::Absent(QUOTE_READ_POSITION_OUT_OF_RANGE))?,
                                ),
                                limit: Some(1),
                            },
                        )
                        .await
                        .map_err(|e| no_row_of_fetch(&e))?;
                    let row = rows
                        .into_iter()
                        .find(|row| row.seq == position)
                        .ok_or(NoRow::Absent(QUOTE_READ_ROW_MISSING))?;
                    let message: RoomMessage = serde_json::from_slice(&row.ciphertext.0)
                        .map_err(|_| NoRow::Failed(QUOTE_READ_DECODE_FAILED))?;
                    Ok::<QuoteRead, NoRow>(QuoteRead::Found(quote_view_of(
                        &row.sender,
                        message.text,
                    )))
                }
                crate::messaging::Surface::Direct(session) => {
                    // A direct log is read forward only, so the row at
                    // `position` sits one page below it; a position with no
                    // page below it has no row, and the read says so.
                    let after_seq = position
                        .checked_sub(1)
                        .filter(|after| *after >= 0)
                        .ok_or(NoRow::Absent(QUOTE_READ_POSITION_OUT_OF_RANGE))?;
                    let rows = client
                        .fetch_direct_messages(session, Some(after_seq), Some(1))
                        .await
                        .map_err(|e| no_row_of_fetch(&e))?;
                    let row = rows
                        .into_iter()
                        .find(|row| row.seq == position)
                        .ok_or(NoRow::Absent(QUOTE_READ_ROW_MISSING))?;
                    let message: DirectMessage = serde_json::from_slice(&row.ciphertext.0)
                        .map_err(|_| NoRow::Failed(QUOTE_READ_DECODE_FAILED))?;
                    Ok::<QuoteRead, NoRow>(QuoteRead::Found(quote_view_of(
                        &row.sender,
                        message.text,
                    )))
                }
            }
        };
        match tokio::time::timeout(QUOTE_READ_TIMEOUT, read).await {
            Ok(Ok(read)) => read,
            Ok(Err(NoRow::Absent(reason))) => {
                warn!(
                    surface = surface.label(),
                    position, reason, "quoted row read found no row"
                );
                QuoteRead::Absent
            }
            Ok(Err(NoRow::Failed(reason))) => {
                warn!(
                    surface = surface.label(),
                    position, reason, "quoted row read failed"
                );
                QuoteRead::Failed
            }
            Err(_) => {
                warn!(
                    surface = surface.label(),
                    position, "quoted row read timed out"
                );
                QuoteRead::Failed
            }
        }
    }

    /// Run a `TagmaRequest` against the root agent and emit the reply, under a
    /// req_id-aware panic boundary. The inbound `SendMessage` is persisted by
    /// the external projector inside `deliver_message` (the shared seam
    /// `execute_op` calls), which also publishes the stamped `UserMessage` frame
    /// the pump forwards — so the app dedups its optimistic user line off that
    /// frame, not this ack. The `MessageAccepted` reply therefore carries
    /// `history_id = 0`. `Interrupt` carries no content and is not stored; its
    /// visible effect is the agent's outbound interrupt event (`Interrupted`, or
    /// `WaitCancelled` when it landed on a parked wait), which the pump already
    /// forwards.
    pub(super) async fn handle_agent_op(
        &self,
        trace: &kallipai_archeion_common::ids::TraceId,
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
                    code: None,
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
    /// always `more=false`. `count` counts the rows emitted; `undecodable`
    /// counts rows the window held that the decoder could not read -- those
    /// are not sent, so a non-zero `undecodable` marks the batch as a window
    /// with a hole in it. A read that cannot be answered emits no marker.
    pub(super) async fn handle_history(
        &self,
        trace: &kallipai_archeion_common::ids::TraceId,
        req_id: u64,
        peer: &Participant,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        // req_id-aware panic boundary (mirrors `handle_agent_op`): a panic in
        // the batch is contained and nothing is emitted. A marker here would
        // report a window that came back empty and whole, which the app folds
        // into its transcript as "no history below this point"; it waits out
        // its deadline instead and retries on reconnect.
        if AssertUnwindSafe(self.handle_history_inner(trace, req_id, peer, after, before, limit))
            .catch_unwind()
            .await
            .is_err()
        {
            error!(req_id, "history batch panicked; no marker");
        }
    }

    async fn handle_history_inner(
        &self,
        trace: &kallipai_archeion_common::ids::TraceId,
        req_id: u64,
        peer: &Participant,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) {
        let limit = limit.min(Self::HISTORY_BATCH_MAX);
        // Read + decode through the external projector, the single history read
        // path shared with the direct `/external/history` endpoint. Returns the
        // decoded page: sender + reply per row, the `more` flag, and how many
        // rows the decoder could not read (they are not in the page, so a
        // non-zero count marks a hole). An Err answer carries no frames at
        // all. Filter to THIS peer's partition so each device sees only its
        // own conversation.
        let Some(projector) = self
            .inner
            .state
            .upgrade()
            .and_then(|s| s.external.get().cloned())
        else {
            // Tagma shutting down: nothing to emit.
            return;
        };
        let page = match projector
            .read_history(Some(peer.id.as_ref()), after, before, limit)
            .await
        {
            Ok(page) => page,
            // An unreadable store, or a query naming two cursors, is not an
            // empty conversation: no marker at all, so the app times out and
            // retries instead of folding an empty window into its transcript.
            Err(e) => {
                error!(req_id, "history read failed: {e}; no marker");
                return;
            }
        };
        let (entries, more, undecodable) = (page.entries, page.more, page.undecodable);
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
        // The count and the cursor travel with the marker: the app sets its
        // badge from this one number instead of counting the rows it
        // receives, and the cursor pairs with it so a snapshot computed
        // earlier cannot overwrite a newer one on arrival order alone.
        let (cursor, unread) = projector.unread_snapshot(Some(peer.id.as_ref())).await;
        if let Err(e) = self
            .emit(
                trace,
                self.agent_sender(),
                TagmaReply::HistoryBatchEnd {
                    req_id,
                    count,
                    more,
                    undecodable,
                    unread,
                    cursor,
                },
                None,
            )
            .await
        {
            warn!(req_id, "history batch-end emit failed: {e:#}");
        }
    }

    /// Translate one op into an in-process tagma call against the root agent and
    /// produce the matching reply, calling the registry directly.
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
            TagmaRequest::SendMessage {
                req_id,
                text,
                attachment,
                quote,
            } => {
                // A quote id that is zero or negative names no row (the
                // client reserves negative ids for lines it has not seen
                // acknowledged). Drop it at the boundary so the stored row,
                // the delivered event, and the ack all report one value.
                let quote = quote.filter(|q| *q > 0);
                let resp = crate::delivery::deliver_message(
                    &state,
                    Identity::Operator,
                    Some(sender),
                    &self.inner.root_agent,
                    crate::delivery::OutgoingMessage {
                        text: &text,
                        attachment: attachment.clone(),
                        quote,
                    },
                    false,
                )
                .await?;
                Ok(TagmaReply::MessageAccepted {
                    req_id,
                    queue_depth: resp.queue_depth,
                    warning: resp.warning,
                    // Intentionally 0: the ack carries no inbound
                    // row id. The pump persists the authored half and
                    // stamps ids onto the Event/UserMessage replies
                    // (relay/pump.rs); the app dedups its optimistic line
                    // on that (op_tests.rs: message ack carries 0).
                    history_id: 0,
                    // Absent for the same reason: the pump stamps the row's
                    // created_at when it persists the authored half.
                    created_at: None,
                    attachment,
                    quote,
                })
            }
            TagmaRequest::Interrupt { req_id } => {
                self.interrupt_root(&state).await?;
                Ok(TagmaReply::Interrupted { req_id })
            }
        }
    }

    /// Cancel the root agent's current round (interrupt). The round form only —
    /// an armed park is the HTTP interrupt route's to cancel, not this
    /// in-process path's. No auth needed — the relay is the trusted
    /// operator-equivalent.
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

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_lesche_common::direct::DirectMessage;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A handle whose lesche client points at `base`. These tests exercise the
    /// one-row read alone, so the rest of `Inner` stays inert: no state (a
    /// dangling `Weak`), no pumps, nothing spawned.
    fn handle(base: &str) -> RelayHandle {
        RelayHandle::new(
            LescheClient::builder(base, "tok")
                .build()
                .expect("lesche client"),
            "test".to_string(),
            TagmaId::from("tagma".to_string()),
            "Tagma".into(),
            DeviceKey::generate(),
            AgentId::from("root".to_string()),
            std::sync::Weak::new(),
        )
    }

    /// One stored room row in the shape `GET /rooms/{room}/messages` returns:
    /// the plaintext `RoomMessage` JSON rides the row's ciphertext field.
    fn room_row(seq: i64, handle: &str, payload: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "seq": seq,
            "sender": {"id": "p-tagma-peer", "kind": "agent", "handle": handle},
            "epoch": 1,
            "ciphertext": serde_json::to_value(Ciphertext(payload.to_vec())).expect("ciphertext"),
            "created_at": "2026-08-02T00:00:00Z",
        })
    }

    /// The same row on a direct session, whose stored view carries no epoch.
    fn direct_row(seq: i64, handle: &str, payload: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "seq": seq,
            "sender": {"id": "p-tagma-peer", "kind": "agent", "handle": handle},
            "ciphertext": serde_json::to_value(Ciphertext(payload.to_vec())).expect("ciphertext"),
            "created_at": "2026-08-02T00:00:00Z",
        })
    }

    fn room_payload(text: &str) -> Vec<u8> {
        serde_json::to_vec(&RoomMessage {
            text: text.to_string(),
            attachment: None,
            quote: None,
            mentions: Vec::new(),
        })
        .expect("room payload")
    }

    fn direct_payload(text: &str) -> Vec<u8> {
        serde_json::to_vec(&DirectMessage {
            text: text.to_string(),
            attachment: None,
            quote: None,
        })
        .expect("direct payload")
    }

    /// The row the position names is the reference: its text, and an author
    /// read off that row's own sender.
    #[tokio::test]
    async fn a_room_quote_renders_the_row_the_position_names() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        let body = serde_json::json!([room_row(
            7,
            "peer-handle",
            &room_payload("hello from the room")
        )]);
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .and(query_param("before_seq", "8"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        let QuoteRead::Found(view) = read else {
            panic!("the row renders: {read:?}");
        };
        assert_eq!(view.text, "hello from the room");
        assert!(
            view.author.contains("peer-handle"),
            "the author names that row's sender: {}",
            view.author
        );
    }

    /// A page that landed on the neighbouring row is not the row asked for: the
    /// read reports no reference instead of pinning the wrong text.
    #[tokio::test]
    async fn a_page_beside_the_position_renders_no_reference() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        let body = serde_json::json!([room_row(6, "peer-handle", &room_payload("the row above"))]);
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Absent),
            "a neighbouring row is not that row: {read:?}"
        );
    }

    /// A fetch that outlasts the budget renders no reference: the row never
    /// arrives, so the reference says it could not be read while the message
    /// itself still goes out.
    #[tokio::test]
    async fn a_slow_fetch_gives_up_the_reference() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        let body = serde_json::json!([room_row(7, "peer-handle", &room_payload("late row"))]);
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(3))
                    .set_body_json(body),
            )
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Failed),
            "a fetch past the budget renders no reference: {read:?}"
        );
    }

    /// A direct log reads forward, so the row at the position is reached from
    /// one below it: one page, one row, the position asked for.
    #[tokio::test]
    async fn a_direct_quote_reads_forward_from_the_row_below() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("session-1".to_string());
        let body = serde_json::json!([direct_row(
            7,
            "peer-handle",
            &direct_payload("hello in private")
        )]);
        Mock::given(method("GET"))
            .and(path("/direct-sessions/session-1/messages"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Direct(&session), 7)
            .await;

        let QuoteRead::Found(view) = read else {
            panic!("the row renders: {read:?}");
        };
        assert_eq!(view.text, "hello in private");
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(
            requests[0].url.query(),
            Some("after_seq=6&limit=1"),
            "one forward page from the row below: {}",
            requests[0].url
        );
    }

    /// A surface that refuses the page costs the reference, never the message.
    #[tokio::test]
    async fn a_failed_fetch_renders_no_reference() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Failed),
            "an unanswered fetch is no reference: {read:?}"
        );
    }

    /// A 404 withholds the row instead of refuting it: the lesche answers it for
    /// an unknown room or session and for a reader who is not a member alike, so
    /// the read learns nothing about whether the row is there.
    #[tokio::test]
    async fn a_surface_that_will_not_show_the_row_leaves_it_unread() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Failed),
            "a 404 withholds the row rather than refuting it: {read:?}"
        );
    }

    /// The direct surface classifies a 404 the same way: the answer withholds
    /// the row, so the read failed instead of finding the log empty.
    #[tokio::test]
    async fn a_direct_surface_that_will_not_show_the_row_leaves_it_unread() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("session-1".to_string());
        Mock::given(method("GET"))
            .and(path("/direct-sessions/session-1/messages"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Direct(&session), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Failed),
            "a 404 withholds the row rather than refuting it: {read:?}"
        );
    }

    /// A row whose payload no longer decodes is not a reference either: the
    /// read would rather say nothing than render bytes it cannot read.
    #[tokio::test]
    async fn a_row_whose_payload_does_not_decode_renders_no_reference() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());
        let body = serde_json::json!([room_row(7, "peer-handle", b"not a message")]);
        Mock::given(method("GET"))
            .and(path("/rooms/room-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), 7)
            .await;

        assert!(
            matches!(read, QuoteRead::Failed),
            "an undecodable row is no reference: {read:?}"
        );
    }

    /// A room position whose page would run past what the log can hold has no
    /// row to fetch: the read says so without asking the surface.
    #[tokio::test]
    async fn a_room_position_with_no_page_above_it_is_not_fetched() {
        let server = MockServer::start().await;
        let room = RoomId::from("room-1".to_string());

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Room(&room), i64::MAX)
            .await;

        assert!(
            matches!(read, QuoteRead::Absent),
            "no page above the position holds no row: {read:?}"
        );
        let requests = server.received_requests().await.expect("requests recorded");
        assert!(
            requests.is_empty(),
            "no request for a position with no page: {requests:?}"
        );
    }

    /// A direct position with no page below it has no row to reach: the read
    /// says so instead of asking the surface for a page below the first row.
    #[tokio::test]
    async fn a_direct_position_with_no_page_below_it_is_not_fetched() {
        let server = MockServer::start().await;
        let session = DirectSessionId::from("session-1".to_string());

        let read = handle(&server.uri())
            .read_quoted_row(Surface::Direct(&session), 0)
            .await;

        assert!(
            matches!(read, QuoteRead::Absent),
            "no page below the position holds no row: {read:?}"
        );
        let requests = server.received_requests().await.expect("requests recorded");
        assert!(
            requests.is_empty(),
            "no request for a position with no page: {requests:?}"
        );
    }
}
