// RoomConversationsStore: the per-room chat state. Rooms BYPASS the bilateral
// projector (the channelsStore / ExternalProjector / chat_history path is
// 1:1-only, per the mellow-baking-taco decision): a room's transcript is
// rendered from the lesche's payload store (`fetchRoomMessages`) + live inbound
// room envelopes, and outbound chat is posted to `/v1/lesche/rooms/{id}/envelopes`.
// Rooms are plaintext server-readable -- the lesche stores + relays the payload
// opaquely and enforces member access; `private` means invite-only, `public`
// means open-access. Both carry the same plaintext `RoomMessage` codec.
//
// A room line is MEMBER-AWARE: each line carries the relay-authenticated sender
// participant id (not a user/assistant role -- rooms are multi-member). `mine`
// flags the caller's own lines for UI alignment; the sender's DISPLAY handle is
// resolved separately (roster / user directory) by the view.

import {
  type Envelope,
  type RoomRosterView,
  type Visibility,
} from "@kallipai/kallipai-lesche-client";
import { decodeB64, encodeB64, uuidV4 } from "@kallipai/kallipai-common";
import { SvelteMap, SvelteSet } from "svelte/reactivity";
import { archeionSession, lescheClientOrFail } from "./archeion.svelte.ts";
import { recentStore } from "./recent.svelte.ts";
import { roomsStore } from "./rooms.svelte.ts";
import { roomKey } from "./unread.svelte.ts";
import {
  room_history_failed,
  rooms_couldnt_reach,
} from "../../paraglide/messages.js";
import {
  appendRoomLine,
  checkedMentions,
  checkedQuote,
  checkedThread,
  decodeRoomMessage,
  dropFailedRoomLine,
  encodeRoomSendMessage,
  mentionsField,
  quoteField,
  roomLineFrom,
  threadField,
  type RoomLine,
  type RoomLineSender,
  type RoomMessage,
  type RoomSendOptions,
} from "../room-message.ts";

// `RoomLine` is re-exported for the view + tests (its pure dedup lives in
// room-message.ts; the store re-exports the type so consumers import from one
// place).
export type { RoomLine };

/** The server's page ceiling for one history request (`HISTORY_MAX_LIMIT` in
 *  chat/rooms.rs). A page shorter than this is the end of the log. */
const HISTORY_PAGE_SIZE = 200;

/** How many pages one automatic walk down a discussion may take before it
 *  stops and asks. A jump to a quoted reply and the walk back to the newest
 *  reply both use it; the reader scrolling page by page is not bounded by it.
 *  The page is the discussion's own rows (the relay filters by `thread`), so
 *  this bounds one long discussion rather than the room. */
export const THREAD_WALK_MAX_PAGES = 20;

type RoomStatus = "loading" | "open" | "error";

/** The per-room conversation state. A `$state.class` (not a plain interface):
 * entries live in a `SvelteMap`, whose values are NOT deep-proxied, so per-field
 * reactivity must come from each entry's own runes -- mirroring `ConversationBase`
 * in session/conversation/base.svelte.ts. In-place field writes
 * (`room.lines = ...`, `room.status = ...`) therefore re-render the view. */
export class RoomConversation {
  readonly roomId: string;
  /** The room's visibility, fixed at open from the registry view. Both
   * `private` (invite-only) and `public` (open-access) rooms carry plaintext
   * payloads; the flag is display semantics only. Defaults to `private` when
   * the registry has not surfaced the room yet. */
  readonly visibility: Visibility;
  lines: RoomLine[] = $state([]);
  /** The highest `seq` rendered so far (the exclusive `after_seq` cursor for
   * the next incremental fetch). Null until the first page lands. */
  lastSeq: number | null = $state(null);
  status: RoomStatus = $state<RoomStatus>("loading");
  error: string | null = $state(null);
  /** Whether the transcript has reached the room's present. The first page
   *  is the OLDEST rows (the route pages forward from 0), so a large room is
   *  behind its tail until the polls walk it forward; a short page proves the
   *  walk finished. A discussion's reply count may be shown as a number only
   *  once this is true: replies always sit after their head, so a window that
   *  reaches the present holds all of them. */
  caughtUp = $state(false);
  /** Whether a line can be written here. The room's carrier is the shared
   *  lesche client rather than a per-room transport, and a send renders an
   *  optimistic line that a failure marks in place for retry -- so the gate is
   *  the last page fetch, not "a route to a tagma is attached". `fetchPage`
   *  sets the status and both `open` and `refresh` call it, so a refresh that
   *  fails closes the composer until one lands again. */
  get historyLoaded(): boolean {
    return this.status === "open";
  }
  /** The latest roster snapshot (members + epoch + is_creator), refreshed by
   * `refreshRoster`. Null until the first refresh lands. The view shows the
   * member count + creator badge; per-member handle resolution is a separate
   * concern. Each member also carries fetch-time `online` (ground truth). */
  roster: RoomRosterView | null = $state(null);
  /** The room's live online-member set (participant ids). The roster re-fetch
   * reconciles it (authoritative); `room_member_online`/`room_member_offline`
   * SSE deltas mutate it between fetches. `SvelteSet` (not `$state(new Set())`):
   * Svelte's `$state` proxy does not wrap Set, so a raw Set's in-place
   * `.add()/.delete()` would be invisible to reactivity. The side panel reads
   * `online.has(memberId)` for each dot. */
  online: SvelteSet<string> = new SvelteSet();
  /** Client-receive time of the last ONLINE delta per member, for the reconcile
   * fence. Internal bookkeeping; not reactive. */
  private deltaAppliedAt = new Map<string, number>();
  /** Monotonic counter for synthetic negative seqs (optimistic sends + live
   * inbound frames that carry no server seq). Decremented per use so every
   * synthetic line gets a distinct key -- deriving it from `lines.length`
   * collides after a `resend` drops a failed line (length shrinks, the next
   * send reuses a seq), and `Date.now()` collides under a same-millisecond
   * burst. Mirrors `ConversationBase.syntheticSeq`. Not reactive. */
  private syntheticSeq = 0;

  /** Next synthetic seq (a distinct negative number). */
  nextSyntheticSeq(): number {
    return (this.syntheticSeq -= 1);
  }

  constructor(
    roomId: string,
    visibility: Visibility,
    /** Fields preserved across an error-state re-open (the prior lines/lastSeq/
     * roster are carried forward so a retry does not blank the view). */
    preserved?: {
      lines?: RoomLine[];
      lastSeq?: number | null;
      roster?: RoomRosterView | null;
      online?: SvelteSet<string>;
    },
  ) {
    this.roomId = roomId;
    this.visibility = visibility;
    this.lines = preserved?.lines ?? [];
    this.lastSeq = preserved?.lastSeq ?? null;
    this.roster = preserved?.roster ?? null;
    // Seed the synthetic-seq counter BELOW any preserved negative seqs: an
    // error-state re-open carries forward prior lines (incl. failed optimistic
    // lines with negative seqs), so the counter must not restart at 0 and reuse
    // a seq still on a preserved line (a Svelte each-key collision). The old
    // length-derived scheme continued past them for free; the explicit counter
    // must too.
    this.syntheticSeq = this.lines.reduce(
      (min, l) => (l.seq < min ? l.seq : min),
      0,
    );
    // Seed online from the carried roster (the fetch-time ground truth) so an
    // error-state re-open does not blank every dot before the next refresh.
    const seed = preserved?.online;
    if (seed && seed.size > 0) {
      for (const id of seed) this.online.add(id);
    } else if (this.roster) {
      for (const m of this.roster.members) if (m.online) this.online.add(m.id);
    }
  }

  /** Adopt a freshly fetched roster and reconcile the live online-member set.
   * `fetchStartedAt` is the instant the fetch began (captured per call, not held
   * on the conv, so two overlapping refreshes keep independent fences). Reconcile,
   * not blind replace: both the online-add and offline-remove are gated by delta
   * recency -- a roster row is applied only when no SSE delta for that member is
   * newer than the fetch start. That preserves a live delta (online OR offline)
   * over a stale in-flight roster response, while the roster still resyncs when
   * it is the freshest signal. Stale delta timestamps for members no longer in
   * the roster are pruned so the map stays bounded. */
  applyRoster(roster: RoomRosterView, fetchStartedAt: number): void {
    this.roster = roster;
    for (const m of roster.members) {
      if ((this.deltaAppliedAt.get(m.id) ?? 0) > fetchStartedAt) {
        continue; // A fresher delta wins over this (possibly stale) roster row.
      }
      if (m.online) {
        this.online.add(m.id);
      } else {
        this.online.delete(m.id);
      }
    }
    // Drop delta timestamps for members who left the roster (snapshot keys
    // first: a Map cannot be mutated while iterated).
    for (const id of [...this.deltaAppliedAt.keys()]) {
      if (!roster.members.some((m) => m.id === id)) {
        this.deltaAppliedAt.delete(id);
      }
    }
  }

  /** Apply a `room_member_online`/`room_member_offline` SSE delta to the live
   * online-member set. The roster re-fetch remains the resync ground truth; this
   * is the live layer between fetches. Both directions stamp `deltaAppliedAt` so
   * `applyRoster` can defer to whichever delta is newest, regardless of
   * direction. */
  applyPresence(memberId: string, online: boolean): void {
    if (online) {
      this.online.add(memberId);
    } else {
      this.online.delete(memberId);
    }
    this.deltaAppliedAt.set(memberId, Date.now());
  }
}

/** Decode a room's plaintext payload (base64 of the UTF-8 `RoomMessage` JSON
 * bytes) into the decoded message op. The lesche stores room payloads opaquely
 * (plaintext, but blind to the codec). */
function decodeRoomPayload(ciphertextB64: string): RoomMessage {
  return decodeRoomMessage(decodeB64(ciphertextB64));
}

/** The room's visibility from the registry view, defaulting to `private` when the
 * registry has not surfaced it. */
function visibilityOf(roomId: string): Visibility {
  return (
    roomsStore.rooms.find((r) => r.room_id === roomId)?.visibility ?? "private"
  );
}

/** A fresh random trace id (the lesche forwards it; rooms ignore it -- no replay
 * window on the room surface -- but the Envelope shape requires a non-empty
 * string). */
function randomTraceId(): string {
  return uuidV4();
}

/** The optimistic own-sender handle: the stable `@username` when resolved, else
 * the degraded `user <prefix>` -- byte-identical to the relay's
 * `degraded_handle` (a 6-char `short_prefix` of the participant id), so the
 * optimistic header and the confirmed history echo agree even in the no-username
 * edge case, and a partially-migrated account never renders a bare `@`. */
function optimisticHandle(participantId: string): string {
  const username = archeionSession.user?.username;
  return username ? `@${username}` : `user ${participantId.slice(0, 6)}`;
}

class RoomConversationsStore {
  // Per-room state, keyed by room id. SvelteMap (not $state(new Map())):
  // Svelte's $state proxy does not wrap Map, so a raw Map's in-place .set() would
  // be invisible to reactivity. SvelteMap tracks entries natively.
  private convs = new SvelteMap<string, RoomConversation>();

  /** Reactive lookup: the conversation for `roomId`, or undefined if never
   * opened. The view derives its phase from `status`. */
  get(roomId: string): RoomConversation | undefined {
    return this.convs.get(roomId);
  }

  /** Ensure a conversation exists for `roomId` + load its history. Idempotent: a
   * no-op re-fetch if already loaded (call `refresh` for incremental). Sets
   * `status` to `loading` while the page is in flight. */
  async open(roomId: string): Promise<void> {
    const existing = this.convs.get(roomId);
    if (existing && existing.status !== "error") return;
    const conv = new RoomConversation(roomId, visibilityOf(roomId), {
      lines: existing?.lines,
      lastSeq: existing?.lastSeq,
      roster: existing?.roster,
      online: existing?.online,
    });
    this.convs.set(roomId, conv);
    await this.fetchPage(roomId);
  }

  /** Incremental fetch: pull rows after `lastSeq` + render + append. Used by
   * the view's poll/refresh (a `room_membership_changed` nudge, or a manual
   * refresh). A no-op if the room was never opened. */
  async refresh(roomId: string): Promise<void> {
    const conv = this.convs.get(roomId);
    if (!conv) return;
    await this.fetchPage(roomId);
  }

  /** Send a chat line: render an optimistic line at once, then post the
   * plaintext payload. The optimistic line uses a synthetic negative seq (the
   * real seq lands on the next history fetch, where the lesche-assigned seq
   * dedups it). On a post failure the optimistic line is marked errored (not
   * removed) so the user can retry. THROWS when no signed-in user is resolved
   * (the room UI surfaces it inline). A mention list that cannot be placed
   * throws here, before the optimistic line exists: nothing is drawn for a
   * send that names nobody. */
  async send(
    roomId: string,
    text: string,
    options?: RoomSendOptions,
  ): Promise<void> {
    const trimmed = text.trim();
    if (!trimmed) return;
    const participantId = archeionSession.participantId;
    if (!participantId) throw new Error("no signed-in user");
    const mentions = checkedMentions(options?.mentions);
    const quote = checkedQuote(options?.quote);
    const thread = checkedThread(options?.thread);
    const conv = this.convs.get(roomId);
    // Optimistic line: synthetic seq below any real (positive) seq. Sender is
    // the participant id (not the raw user_id) so the history echo dedups it.
    const optimistic: RoomLine = {
      seq: conv ? conv.nextSyntheticSeq() : -1,
      senderId: participantId,
      // The local app user is the human participant. Own bubbles render the
      // same display-name + @handle header as everyone else (no "you"), so the
      // optimistic line carries the user's own stable `@username` handle -- the
      // same shape the relay stamps on the confirmed echo (`@<username>`), so
      // the header is correct from the instant it appears and stays identical
      // when history reconciles it. Falls back to the degraded `user <prefix>`
      // (mirroring the relay's `degraded_handle`) only when no username is
      // resolved, never a bare `@`.
      senderKind: "human",
      senderHandle: optimisticHandle(participantId),
      text: trimmed,
      createdAt: new Date().toISOString(),
      mine: true,
      ...mentionsField(mentions),
      ...quoteField(quote),
      ...threadField(thread),
    };
    if (conv) conv.lines = [...conv.lines, optimistic];
    try {
      const payload = encodeB64(
        new TextEncoder().encode(
          encodeRoomSendMessage(trimmed, { mentions, quote, thread }),
        ),
      );
      await lescheClientOrFail().postRoomEnvelope(
        roomId,
        this.buildEnvelope(roomId, payload),
      );
      // The room moved: recents order by this on its own, so a room read to
      // the end (no badge left to fall) still climbs when something lands.
      recentStore.note(roomKey(roomId));
      // Confirm the optimistic line without waiting for the next poll: the
      // lesche fan excludes the sender, so my own echo only arrives via a
      // history fetch (which dedups the pending line by sender + text).
      void this.refresh(roomId);
    } catch (e) {
      // Transient POST failure: mark the optimistic line failed and rethrow.
      // The line is NOT removed (dropping it silently would lose the user's
      // input) and the conversation stays `open` -- a send failure is not a
      // terminal error, so the poll + composer keep running and the view offers
      // a retry on the failed line.
      if (conv) {
        conv.lines = conv.lines.map((l) =>
          l.seq === optimistic.seq ? { ...l, failed: true } : l,
        );
      }
      throw e;
    }
  }

  /** Retry a failed optimistic line: drop it, then re-send its text through the
   * normal send path (fresh optimistic line + POST). The failed line is removed
   * first so the retry does not duplicate it. THROWS on failure (the view
   * surfaces it), and the retry keeps the names and the quote the failed send
   * carried -- a retried reply must not turn into an unquoted one. */
  async resend(roomId: string, line: RoomLine): Promise<void> {
    const conv = this.convs.get(roomId);
    if (!conv) return;
    conv.lines = conv.lines.filter((l) => l.seq !== line.seq);
    await this.send(roomId, line.text, {
      mentions: line.mentions,
      quote: line.quote,
      thread: line.thread,
    });
  }

  /** Discard a failed own line for good (the bubble's delete action). Unlike
   * the bilateral path there is no durable pending row to remove and no
   * partition that could re-read it: a room send posts straight through and a
   * failure only marks the optimistic line, so dropping the line is the whole
   * of it. Only a failed line is discardable -- a pending one is still on its
   * way, and a line with a server seq is history. Returns whether it left, so
   * the caller can place focus before the row disappears. */
  discard(roomId: string, seq: number): boolean {
    const conv = this.convs.get(roomId);
    if (!conv) return false;
    const lines = dropFailedRoomLine(conv.lines, seq);
    if (!lines) return false;
    conv.lines = lines;
    return true;
  }

  /** Render a live inbound room envelope (delivered by the realtime SSE
   * `envelope` event after the sink demuxes room vs bilateral). A no-op if the
   * room was never opened. */
  deliverLive(
    roomId: string,
    ciphertextB64: string,
    sender: RoomLineSender,
  ): void {
    const conv = this.convs.get(roomId);
    if (!conv) return;
    this.renderPublic(conv, ciphertextB64, sender, /*seq*/ null);
  }

  /** Read ONE page of a discussion: the rows the server returns for a cursor,
   *  plus the cursor the next page starts from. The panel opens on the page
   *  the root sits on and walks down from there, one page per call.
   *
   *  `lastSeq` is the highest seq among the page's RAW rows -- the cursor for
   *  the next page -- and `null` for an empty page. It is read off what the
   *  server returned, not off the lines kept below: a row the fallback filter
   *  drops still moves the cursor, so the next page cannot return it twice and
   *  the walk cannot stall on a page it kept nothing from. `hasMore` is true
   *  exactly when the page came back full.
   *
   *  The stored column is the fact and the payload's declaration is the
   *  fallback, so a reply to a reply lands under the discussion's head either
   *  way. The relay is asked to filter by the discussion (`thread: root`); the
   *  local check stays as the guard for a relay that predates the parameter,
   *  where the same walk returns room rows instead. Does not touch the room's
   *  transcript state: a panel reading a discussion is not the room advancing
   *  past what it rendered. */
  async fetchThreadPage(
    roomId: string,
    root: number,
    afterSeq: number,
  ): Promise<{ lines: RoomLine[]; lastSeq: number | null; hasMore: boolean }> {
    const selfId = archeionSession.participantId;
    const lines: RoomLine[] = [];
    // A root below 1 names no row: seqs start at 1, and a stored root is a seq,
    // so an empty discussion is what the server would answer. The request
    // would be refused instead, so it is not sent.
    if (!Number.isInteger(root) || root < 1) {
      return { lines, lastSeq: null, hasMore: false };
    }
    const rows = await lescheClientOrFail().fetchRoomMessages(roomId, {
      afterSeq,
      thread: root,
      limit: HISTORY_PAGE_SIZE,
    });
    let lastSeq: number | null = null;
    for (const row of rows) {
      lastSeq = lastSeq === null ? row.seq : Math.max(lastSeq, row.seq);
      try {
        const decoded = decodeRoomPayload(row.ciphertext);
        if (decoded.op !== "message") continue;
        const thread = row.thread ?? decoded.thread ?? null;
        if (row.seq !== root && thread !== root) continue;
        lines.push(
          roomLineFrom(decoded, row.sender, {
            seq: row.seq,
            createdAt: row.created_at,
            selfId,
            thread: row.thread,
          }),
        );
      } catch (e) {
        // A row whose payload does not decode loses that row, never the page,
        // and the cursor below has already moved past it.
        console.error("[room thread] decode failed:", e);
      }
    }
    return { lines, lastSeq, hasMore: rows.length >= HISTORY_PAGE_SIZE };
  }

  /** Drop a room's state (on leave). */
  dispose(roomId: string): void {
    this.convs.delete(roomId);
  }

  /** Drop all room state (logout). A prior user's rendered room lines are
   * plaintext; they must not survive into the next session on a shared device
   * (the room conversation ids differ per user, but a stale `open()` would
   * short-circuit and leak the previous transcript). */
  reset(): void {
    this.convs.clear();
  }

  /** Create a room. Delegates the lesche create + registry prepend + busy
   * surface to `roomsStore.createRoom(opts)` (which throws on failure). Returns
   * the created room id. */
  async createRoom(opts: {
    name: string;
    description?: string;
    visibility?: Visibility;
  }): Promise<string> {
    const room = await roomsStore.createRoom(opts);
    return room.room_id;
  }

  /** Refresh a room's roster snapshot for display (member count + creator
   * badge) and reconcile the live online-member set. Best-effort: a fetch
   * failure leaves the prior roster in place (the next poll retries). Driven by
   * the room page's poll and the `room_membership_changed` SSE nudge. */
  async refreshRoster(roomId: string): Promise<void> {
    const conv = this.convs.get(roomId);
    if (!conv) return;
    // Capture the fence per call (not on the conv) so overlapping refreshes --
    // the 10s poll vs. a `room_membership_changed` nudge vs. the open-effect's
    // initial fetch -- keep independent fences; a stale response from one cannot
    // darken a dot a fresher delta lit under another.
    const fetchStartedAt = Date.now();
    let roster: RoomRosterView;
    try {
      roster = await lescheClientOrFail().fetchRoomRoster(roomId);
    } catch {
      return; // roster fetch failed; the next poll retries
    }
    conv.applyRoster(roster, fetchStartedAt);
  }

  /** Apply a `room_member_online`/`room_member_offline` SSE delta to a room's
   * live online-member set. A no-op if the room is not open. The roster re-fetch
   * remains the resync ground truth; this is the live layer between fetches. */
  applyMemberPresence(roomId: string, memberId: string, online: boolean): void {
    this.convs.get(roomId)?.applyPresence(memberId, online);
  }

  /** Build a room envelope carrying `payload` (the base64 plaintext). The
   * `channel_id` IS the room id (the lesche route path is authoritative;
   * this must match it); `sequence_n` is 0 (the rooms route ignores it);
   * `trace_id`/`timestamp` are required-shape but not load-bearing for rooms.
   * `sender.id` MUST be the participant id -- the lesche's envelope route
   * rejects a sender that does not equal the authed principal's derived id.
   * `sender.handle` is advisory only: the relay overwrites it with the stable
   * `@username` handle before persist + fan-out, so this value never labels
   * anything (the optimistic local line already carries the user's own
   * `@username`). THROWS when no signed-in user is resolved. */
  private buildEnvelope(roomId: string, payload: string): Envelope {
    const participantId = archeionSession.participantId;
    if (!participantId) throw new Error("no signed-in user");
    const user = archeionSession.user;
    return {
      channel_id: roomId,
      sender: {
        id: participantId,
        kind: "human",
        handle: user?.display_name ?? user?.username ?? "",
      },
      sequence_n: 0,
      trace_id: randomTraceId(),
      timestamp: new Date().toISOString(),
      ciphertext: payload,
    };
  }

  // -- internals -------------------------------------------------------------

  /** Fetch one page of room history (after `lastSeq`) + render + append the
   * new lines. Sets status to `open` on success or `error` on failure. */
  private async fetchPage(roomId: string): Promise<void> {
    const conv = this.convs.get(roomId);
    if (!conv) return;
    try {
      const rows = await lescheClientOrFail().fetchRoomMessages(roomId, {
        afterSeq: conv.lastSeq ?? undefined,
      });
      for (const row of rows) {
        this.renderPublic(
          conv,
          row.ciphertext,
          row.sender,
          row.seq,
          row.created_at,
          row.thread,
        );
      }
      conv.status = "open";
      conv.caughtUp = rows.length < HISTORY_PAGE_SIZE;
      conv.error = null;
    } catch (e) {
      conv.status = "error";
      console.error("[room history] fetch failed:", e);
      conv.error = rooms_couldnt_reach();
    }
  }

  /** Render one room payload (plaintext) as a line. `sender` is the
   * relay-authenticated envelope sender (id + kind + authoritative handle, and
   * -- for an agent -- its `tagma_id`) stamped on the stored row / live
   * envelope. Dedup by seq + advance the lastSeq cursor; a malformed payload is
   * warn-dropped, not thrown -- one bad row must not blank the page. */
  private renderPublic(
    conv: RoomConversation,
    ciphertextB64: string,
    sender: RoomLineSender,
    seq: number | null,
    createdAt?: string,
    thread?: number | null,
  ): void {
    if (seq !== null && conv.lastSeq !== null && seq <= conv.lastSeq) return;
    let decoded;
    try {
      decoded = decodeRoomPayload(ciphertextB64);
    } catch (e) {
      console.error("[room history] decode failed:", e);
      conv.error = room_history_failed();
      return;
    }
    if (decoded.op !== "message") return;
    const me = archeionSession.participantId;
    const line = roomLineFrom(decoded, sender, {
      seq: seq ?? conv.nextSyntheticSeq(),
      createdAt: createdAt ?? new Date().toISOString(),
      selfId: me,
      thread,
    });
    this.commitLine(conv, line, seq);
  }

  /** Append a built line + advance the dedup cursor. */
  private commitLine(
    conv: RoomConversation,
    line: RoomLine,
    seq: number | null,
  ): void {
    conv.lines = appendRoomLine(conv.lines, line);
    if (seq !== null && (conv.lastSeq === null || seq > conv.lastSeq)) {
      conv.lastSeq = seq;
    }
  }
}

export const roomConversationsStore = new RoomConversationsStore();
