// What one authored frame MEANS for a conversation: the in-flight send's ack,
// a replay's adopt, the reduce itself, the cache row and the cursor step.
//
// The order these rules run in sits in one function, so the order can be
// driven with no conversation, store or stream behind it.
// The leaf answers the three things only it knows: how to mint a synthetic id,
// what a gap in positions should repair, and what to do after each reply.

import type { Participant, TagmaReply } from "@kallipai/kallipai-lesche-client";
import {
  applyTagmaReply,
  cacheLineOf,
  isInFlightError,
  markLineSent,
  replaceLineId,
  stampPosition,
  toSender,
} from "../../transcript.ts";
import { sendFailureCopy } from "../../errors.ts";
import { deletePending } from "@kallipai/kallipai-keryx-client";
import type { Transcript } from "./transcript.svelte.ts";
import type { Outbox } from "./outbox.svelte.ts";
import { contentId, contentSeq, putCacheLine } from "./shared.ts";

/** What {@link applyReply} reduces against: the read model, the send queue,
 *  and the leaf's own answers. */
export interface FrameTarget {
  readonly transcript: Transcript;
  readonly outbox: Outbox;
  readonly cacheConversationId: string;
  /** The pending-store partition unsent lines live under. A method, not a
   *  value: a leaf builds its key from fields the base constructor cannot
   *  see yet. */
  pendingPartition(): string;
  /** Mint a synthetic (client-side, negative) row id for a line the wire
   *  did not stamp. */
  nextSyntheticId(): number;
  /** Repair the log above the cursor: a frame that jumps leaves a hole, and
   *  the pull is how the missing rows come back. */
  repair(): void;
  /** The leaf\'s reaction after every reply, admitted or not. */
  after(reply: TagmaReply): void;
}

/** Apply one authored reply: dedup by `history_id`, close or promote the
 *  in-flight optimistic line, adopt a replay, reduce, cache, and step the
 *  cursor. `sender` is the wire participant who authored the reply\'s
 *  content. Callers guard on liveness so a stale drain cannot touch a fresher
 *  entry. */
export function applyReply(
  target: FrameTarget,
  reply: TagmaReply,
  sender: Participant | undefined,
): void {
  // A `user_message` echo closes the in-flight optimistic line -- but only
  // when it is THIS send's echo. Correlate by text: a history-replay
  // `user_message` (e.g. a relay catch-up row) arriving while a fresh local
  // send is in-flight must NOT be consumed as the ack (the echo carries no
  // req_id to correlate on). A stamped echo (history_id > 0) promotes the
  // line to the durable id; an unstamped echo (history_id === 0, a
  // direct-path echo or a DB-write failure) keeps the synthetic id and just
  // flips "sending" -> "sent". Either way the echo is consumed (never
  // appended as a duplicate) and the send pump advances. A non-matching echo
  // falls through to the normal dedup/append path below. Residual edge: a
  // catch-up row whose text identically matches the in-flight send still
  // collides -- far rarer than the prior uncorrelated misfire.
  // An op error answering the in-flight send (correlated by req_id):
  // the message never landed -- e.g. the peer tagma is parked -- so close
  // the optimistic line with our own sentence, release the pump slot, and
  // let the code pick the line's short form. Unmatched op errors keep the
  // path below: a durable system line + red, the server-error record.
  if (isInFlightError(reply, target.outbox.pendingInFlight)) {
    const localId = target.outbox.pendingInFlight!.localId;
    const failure = sendFailureCopy(reply);
    target.outbox.failInFlight(localId, failure.copy, failure.code);
    // The frame's own sentence stays in the console; the line shows our copy,
    // the short form keyed by the code when the frame named one.
    console.warn(`[send] rejected (${reply.status}):`, reply.message);
    target.after(reply);
    return;
  }
  // A frame this window already renders is a replay, whatever else is in
  // flight: its row is on screen, so it never claims a queued line (two
  // identical texts would otherwise hand the echo to the wrong one).
  const realId = contentId(reply);
  const replayed =
    realId > 0 &&
    target.transcript.state.lines.some((l) => l.historyId === realId);
  if (replayed) {
    // The row is on screen already: the frame only hands it its position.
    target.transcript.adopt(reply, realId, sender);
    target.after(reply);
    return;
  }
  if (
    reply.kind === "user_message" &&
    target.outbox.pendingInFlight !== null &&
    reply.text.trim() === target.outbox.pendingInFlight.text.trim()
  ) {
    const localId = target.outbox.pendingInFlight.localId;
    const ackId = reply.history_id;
    if (ackId > 0) {
      // The wire sender is authoritative for the confirmed line: overwrite the
      // optimistic line's (stale, client-side) sender so a handle that changed
      // mid-session does not freeze on the old value, matching every other path
      // where the wire sender drives the rendered/cached sender.
      const wireSender = sender ? toSender(sender) : undefined;
      target.transcript.state = replaceLineId(
        target.transcript.state,
        localId,
        ackId,
        reply.created_at ?? undefined,
        wireSender,
      );
      target.transcript.adopt(reply, ackId, sender);
      if (ackId > target.transcript.maxRowId)
        target.transcript.maxRowId = ackId;
      // The send landed: the durable pending copy is no longer needed.
      void deletePending(target.pendingPartition(), localId);
    } else {
      // Unstamped echo: keep the synthetic line, flip it to "sent". The echo
      // carries no new content, so drop it (never append) -- otherwise it
      // would render a second bubble and the send pump would never advance.
      target.transcript.state = markLineSent(target.transcript.state, localId);
      // Landed (direct-path ack): drop the durable pending copy too.
      void deletePending(target.pendingPartition(), localId);
    }
    target.outbox.releaseInFlight();
    target.after(reply);
    return;
  }
  // Dedup: a frame below the window's floor was rendered and then let go.
  // Anything between that floor and the newest row is a row this
  // conversation never landed (a settled line can leave such a hole), and
  // it lands now.
  if (
    target.transcript.minRowId > 0 &&
    realId > 0 &&
    realId < target.transcript.minRowId
  ) {
    target.after(reply);
    return;
  }
  const seq = contentSeq(reply);
  // A position at or below the cursor is already covered: the row is on
  // screen, or was before paging let it go, so the frame is a replay and
  // nothing about it may move the cursor again.
  // A frozen window has no cursor to compare against: its positions come
  // from another numbering, so frames render and only row ids dedup.
  if (
    !target.transcript.frozen &&
    seq !== null &&
    seq <= target.transcript.lastSeq
  ) {
    target.after(reply);
    return;
  }
  const lineId = realId > 0 ? realId : target.nextSyntheticId();
  target.transcript.state = applyTagmaReply(
    target.transcript.state,
    reply,
    sender,
    lineId,
  );
  // The position a frame carries lands with the line it renders: a row that
  // arrived on the stream is as placeable as one a page delivered, so a
  // later page merging this id has nothing left to fill in.
  if (!target.transcript.frozen && seq !== null) {
    target.transcript.state = stampPosition(
      target.transcript.state,
      lineId,
      seq,
    );
  }
  const cl = cacheLineOf(reply, sender);
  // A frozen window keeps rows off the cache: a cached row carries a
  // position, and this build cannot tell which numbering minted it.
  if (cl) {
    if (!target.transcript.frozen) {
      putCacheLine(target.cacheConversationId, cl);
    }
    if (cl.historyId > target.transcript.maxRowId)
      target.transcript.maxRowId = cl.historyId;
  }
  // Move the resume cursor over the position this frame carries. A frame
  // that jumps above the cursor still renders (its content is real), but
  // the cursor stays below the hole so one repair pulls the missing rows
  // back; an unstamped frame moves nothing.
  if (!target.transcript.frozen && seq !== null) {
    if (seq === target.transcript.lastSeq + 1) target.transcript.advance([seq]);
    else if (seq > target.transcript.lastSeq + 1) target.repair();
  }
  target.after(reply);
}
