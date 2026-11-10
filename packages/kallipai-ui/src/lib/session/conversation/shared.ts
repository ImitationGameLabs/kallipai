// Shared vocabulary for the conversation pieces: the transport-status type the
// store and the composer read, the page size, and the two tests that every
// reply path uses to tell content from protocol traffic.
//
// The pieces themselves live beside this file:
//   base.svelte.ts        the conversation shell: the store contract, send,
//                         and one constructor that wires the owners
//   relayConversation.svelte.ts    the online leaf: the E2EE pipe
//   localConversation.svelte.ts    the direct leaf: the local socket stream
//   offlineConversation.svelte.ts  the degraded view over the cache
//   transcript.svelte.ts  the rendered rows and the positions they carry
//   outbox.svelte.ts      user lines that have not landed yet
//   pager.svelte.ts       asking a page source for rows
//   link.svelte.ts        the read path's connection
//   frames.ts             one reply folded into the rows and the outbox
//
// Nothing here holds state: the exports are types, constants, pure functions,
// and the one function that writes a cached row.
//
// --- The ordering rules these pieces implement.
//
// 1. Send pump: pendingInFlight is set BEFORE the POST; an echo is
//    correlated by text because the wire does not echo req_id; a settled
//    response takes its id directly and never moves the position cursor.
// 2. Send watchdog: 30s, aligned with the SSE connect timeout and the
//    per-attempt fetch cap; expiry marks the line failed (retryable) and
//    never re-sends on its own.
// 3. Pending store: retryAllPending renders in stored order (the order the
//    lines were written); queued rows re-send automatically (at-least-once),
//    failed rows wait for the user; discarded local ids veto a row that
//    arrives from a read taken before the delete landed.
// 4. Positions advance only over a contiguous run (seq === lastSeq + 1); a
//    gap triggers one repair; equality or below is a replay. A page's `next`
//    moves the request anchor, never the cursor.
// 5. The row-id bracket moves only when a row lands; recomputing the floor is
//    idempotent and only moves down. A pager stops for two different reasons:
//    `!more` (the tagma has nothing older) and `added === 0` (this page could
//    not move the window).
// 6. Adopting the newest window is one pull, one fold, one badge derivation;
//    rows that do not touch the window are pruned before folding, and a
//    cached page is spliced only when it abuts the window head.
// 7. Relay content frames carry no request id: one before-page is in flight
//    at a time, the catch-up window owns every content frame while it is in
//    flight, and a page buffer is registered before the request is issued.
// 8. The read cursor is reported only while the page is mounted, coalesced
//    into one PUT per throttle window; only a landed write advances the
//    reported watermark.
// 9. Every drain mutation is guarded by an object-identity check against the
//    store's entry for this id (a reconnect replaces the entry).
// 10. A window frozen by a numbering mismatch renders what arrives and places
//     nothing: no position stamp, no cursor move, no cache row, no repair.
// 11. A cache hydrate comes in two shapes, and the split is deliberate:
//     the online leaf restores the rows together with the resume cursor its
//     stream resumes from, the offline view restores the rows alone (it has
//     no stream of its own to resume).

import type { TagmaReply } from "@kallipai/kallipai-lesche-client";
import { put as cachePut } from "@kallipai/kallipai-keryx-client";
import type { CachedLine } from "@kallipai/kallipai-keryx-client";
import type { ConversationLine } from "../../transcript.ts";

/** Transport-status surface: the sidebar dot + the chat-page disabled gate.
 *  Distinct from the transcript's own `status` (idle/busy/error, which is the
 *  send banner of the last optimistic line). */
export type LinkStatus =
  | "opening"
  | "open"
  | "reconnecting"
  | "offline"
  | "error";

/** The lazy-window page size: how many lines a hydrate, a catch-up batch,
 *  or a scroll-up page brings in at once. Mirrors the server's
 *  /external/history clamp (50: DEFAULT_LIMIT = MAX_LIMIT, routes/message.rs)
 *  so a full page is one request — if that clamp ever moves, this mirror
 *  drifting only costs an extra partial request, never correctness. */
export const WINDOW_PAGE = 50;

/** Map a cached row back to its transcript line (the hydrate + cache-page
 *  paths; the cache stores the UI's own role union verbatim, cast back to
 *  the reducer's role union, which the values round-trip as). */
export function cachedLineToLine(c: CachedLine): ConversationLine {
  return {
    historyId: c.historyId,
    role: c.role as ConversationLine["role"],
    text: c.text,
    sender: c.sender,
    createdAt: c.createdAt,
    attachment: c.attachment,
    quote: c.quote,
    seq: c.seq,
  };
}

/** Write one rendered line into the conversation's cache: the single place a
 *  cached row is minted, so the live drain, a pulled page and a hydrate cannot
 *  drift apart in the fields a replay needs. Best-effort by design (a private
 *  window has no IndexedDB and the write swallows). */
export function putCacheLine(
  conversationId: string,
  line: ConversationLine,
): void {
  void cachePut({
    conversationId,
    historyId: line.historyId,
    role: line.role,
    text: line.text,
    sender: line.sender,
    createdAt: line.createdAt,
    attachment: line.attachment,
    quote: line.quote,
    seq: line.seq,
  });
}

/** The durable id a content frame carries (0 for markers, acks, and system
 *  lines): the single test every reducer path uses to tell content from
 *  protocol traffic. */
export function contentId(reply: TagmaReply): number {
  return reply.kind === "event" || reply.kind === "user_message"
    ? (reply.history_id ?? 0)
    : 0;
}

/** The position a content frame carries (null for markers, acks, and frames
 *  the tagma did not stamp): the content test of {@link contentId}, in the
 *  numbering the resume cursor lives in. */
export function contentSeq(reply: TagmaReply): number | null {
  const seq =
    reply.kind === "event" || reply.kind === "user_message" ? reply.seq : null;
  return seq != null && seq > 0 ? seq : null;
}
