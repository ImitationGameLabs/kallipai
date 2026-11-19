// UnreadStore: the per-conversation unread-badge state.
// One entry per conversation key -- `room:{room_id}` for multi-member rooms,
// `tagma:{tagma_id}` for bilateral 1:1 chats -- tracking:
//
//   count      the number the badge shows (rendered through {@link badgeLabel},
//              which caps the display at "99+").
//   knownSeq   rooms only: the highest line seq this session has SEEN (the room
//              page's rendered lines, and the enter/leave sync). The cursor
//              write reads it, so opening a room reports how far the
//              transcript got.
//   readSeq    rooms only: the server-acknowledged read cursor
//              (room_read_cursors): the room list's value, a cursor echo, or
//              the cursor a badge read came back with.
//
// Neither number is derived on the device. A room badge is the lesche's answer
// to "how many rows past my cursor reached me" (`GET /rooms/{id}/unread`), and
// a 1:1 badge is the tagma's stamped count. The store asks and assigns, so a
// device syncing a long backlog shows one number instead of a counter ticking
// per landed row, and a room whose history was never pulled still badges
// correctly.
//
// Why the count lives on the server: the badge says what REACHED the member,
// and a reply inside a thread reaches only that thread's agents (every human
// is reached either way). Delivery reads the thread's participant table, so the
// count has to read it too; a device counting pulled rows would count rows it
// was never sent, and history pulled before a join is not what reached this
// member.
// `store::unread_for_member` carries the exact predicate and where it approximates.
//
// When the store asks for a room count:
//   - cold start: the room list seeds the watermarks and one count per room --
//     the same one-request-per-room volume the catch-up page had;
//   - a room envelope arrives (RootLayout's demux calls
//     {@link noteRoomActivity}): one count, single-flight with a trailing pass,
//     so a burst of messages lands on the last truth and overhead stays at one
//     request per round trip;
//   - another session moves the cursor (a `room_read_cursor_changed` echo, or
//     a fresher list watermark): one count at once, because a quiet room has no
//     next envelope to carry the correction;
//   - the room page closes: the cursor write flushes first, then one count.
// Nothing is counted locally, so there is no dedup fence to keep and no
// double-count to avoid: two answers for one room replace each other.
//
// 1:1 keeps no local tally at all: the tagma owns the number. It counts the
// outbound rows past this user's read cursor in its own store and stamps the
// result -- when it appends a reply and when the cursor moves -- through three
// carriers:
//   - the `history_batch_end` marker's `unread` (every catch-up window and
//     before-page answers with the count as of that answer);
//   - a live relay frame the keryx surfaces as `tagma_unread` on this user's
//     own app stream;
//   - the keryx's cached copy, flushed on `/me/events` connect, so a cold
//     client badges before its first conversation opens.
// The store only ASSIGNS those numbers ({@link setTagmaUnread}): a device
// syncing a long history shows one number, never a counter ticking per landed
// row.
//
// The other direction is the read cursor: the client reports how far it has
// read and the tagma stamps the resulting count (again through
// {@link setTagmaUnread}). Reporting is plumbing, not counting --
// RelayConversation sends the id of the newest row it renders, on page mount
// and per live line while the page stays mounted; the store reads back only
// what comes stamped.
//
// Cold start is the third carrier's job: the keryx keeps one cached snapshot
// per tunnel and the tagma re-announces every partition as its upstream link
// comes up, so `/me/events` opens with one frame per tagma. The hub's badge is
// then a sum of assigned numbers -- no local accumulation and no
// per-conversation open required.
//
// Metadata note: a plaintext upstream carrying `cursor`/`unread` lets the relay
// observe how far this user has read in a 1:1 -- the same trade the room cursor
// already makes (room watermarks are server-side too). Deliberate, and scoped:
// those frames reach the user's own app stream only, never the peer tagma or
// any other principal.
//
// Viewing discipline: only an explicitly opened conversation page is
// "viewing" -- boot auto-open never calls enter(), so a login can never
// silently clear badges. While viewing, the badge shows 0 and no room count is
// asked for: the reader is looking at the rows, so an answer nobody would see
// is one request too many. The room page's line tick ({@link noteViewedLines})
// advances knownSeq and coalesces the cursor write into one request per
// PUT_THROTTLE_MS window (a busy chat must not POST per line); the leave path
// flushes once and asks for the count after the flush, which is where a long
// viewing session's number comes from. Scope note: viewing keys on the page
// being mounted, not on document visibility; a background tab with the room
// page open still counts as reading it (the notification layer -- notify.ts --
// owns hidden-window semantics).
//
// The client-side defense lives in putRoomCursor: only a safe non-negative
// integer the client actually observed is ever sent -- the store never invents
// a value the server's max-clamp would pin irreversibly (and a late lower write
// cannot regress the cursor anyway: the server clamps to max).
import { SvelteMap } from "svelte/reactivity";
import {
  fetchRoomUnread,
  setRoomReadCursor,
} from "@kallipai/kallipai-lesche-client";
import { runClientEffect } from "@kallipai/kallipai-common";
import { lescheBaseUrlOrFail } from "./archeion.svelte.ts";

/** Badge display cap: anything above renders as "99+" ({@link badgeLabel}). */
export const UNREAD_CAP = 99;
/** While viewing, room cursor writes coalesce into one request per window. */
export const PUT_THROTTLE_MS = 5000;

export function roomKey(roomId: string): string {
  return `room:${roomId}`;
}
export function tagmaKey(tagmaId: string): string {
  return `tagma:${tagmaId}`;
}

/** The badge label for a count: the number itself up to the cap, "99+"
 *  beyond. Pure so the cap shape is testable without a component. */
export function badgeLabel(count: number): string {
  return count > UNREAD_CAP ? "99+" : String(count);
}

/** One unread stamp from a server: the count and the read cursor it was
 *  measured against. One object because the pair is the ordering fact -- a
 *  count without its cursor cannot be placed against another stamp, so the
 *  carriers hand over the whole thing or nothing. */
export interface UnreadStamp {
  readonly unread: number;
  readonly cursor: number;
}

/** One conversation's unread state. Per-field `$state` runes (not a plain
 *  object): SvelteMap values are not deep-proxied, so in-place field writes
 *  must carry their own reactivity (mirrors RoomConversation). */
export class UnreadEntry {
  count = $state(0);
  knownSeq = $state(0);
  readSeq = $state(0);
  /** 1:1 only: the read cursor the current count was stamped against. A
   *  stamp computed before a cursor write landed carries a smaller cursor
   *  than one computed after it, so the pair orders the stamps even when two
   *  carriers race on their way here. */
  stampCursor = 0;
  /** 1:1 only: the tagma's own number, never zeroed by viewing. {@link count}
   *  is what the badge shows (0 while the page is mounted); this is the fact
   *  the next stamp has to beat, so opening the page cannot lower the bar a
   *  later stamp has to clear. */
  stampCount = 0;
  /** Whether a count has ever reached this device for the key. `enter()` and
   *  `initRooms` seed an entry with a zero badge, and a seed is not a fact:
   *  without this flag the replay after opening a page would beat that zero
   *  and stamp days-old backlog as an arrival. */
  stampSeen = false;

  constructor(count: number, knownSeq: number, readSeq: number) {
    this.count = count;
    this.knownSeq = knownSeq;
    this.readSeq = readSeq;
  }
}

type RoomUnreadFetcher = (roomId: string) => Promise<UnreadStamp>;
type ReadCursorPutter = (roomId: string, seq: number) => Promise<void>;
type TimerArmer = (fn: () => void) => ReturnType<typeof setTimeout>;

function defaultFetchRoomUnread(roomId: string): Promise<UnreadStamp> {
  return runClientEffect(fetchRoomUnread(lescheBaseUrlOrFail(), roomId));
}
function defaultPutCursor(roomId: string, seq: number): Promise<void> {
  return runClientEffect(setRoomReadCursor(lescheBaseUrlOrFail(), roomId, seq));
}
function defaultArmTimer(fn: () => void): ReturnType<typeof setTimeout> {
  return setTimeout(fn, PUT_THROTTLE_MS);
}

class UnreadStore {
  private entries = new SvelteMap<string, UnreadEntry>();
  /** Keys whose conversation page is explicitly mounted (viewing). */
  private viewing = new Set<string>();
  // Room count scheduler: single-flight with a trailing pass.
  private countInFlight = new Set<string>();
  private countDirty = new Set<string>();
  // Room cursor-write throttle: pending writes by room + the window timer.
  private putPending = new Map<string, number>();
  private timerArmed = false;
  private timerId: ReturnType<typeof setTimeout> | null = null;
  // Test seams (null restores the production default).
  private fetchRoomUnread: RoomUnreadFetcher = defaultFetchRoomUnread;
  private putCursor: ReadCursorPutter = defaultPutCursor;
  private armTimer: TimerArmer = defaultArmTimer;

  setRoomUnreadFetcher(f: RoomUnreadFetcher | null): void {
    this.fetchRoomUnread = f ?? defaultFetchRoomUnread;
  }
  setReadCursorPutter(f: ReadCursorPutter | null): void {
    this.putCursor = f ?? defaultPutCursor;
  }
  setTimerArmer(f: TimerArmer | null): void {
    this.armTimer = f ?? defaultArmTimer;
  }

  /** Seed room entries from the registry list (the list_rooms response
   *  carries the caller-scoped last_read_seq). First sight seeds the
   *  watermarks and asks for the count; a re-list only advances a server
   *  watermark that another session moved, which asks again (the list is the
   *  resync ground truth). Rooms gone from the registry drop their entry
   *  (member removal / room deletion). */
  initRooms(rows: { roomId: string; lastReadSeq: number }[]): void {
    const seen = new Set<string>();
    for (const r of rows) {
      const key = roomKey(r.roomId);
      seen.add(key);
      const entry = this.entries.get(key);
      if (!entry) {
        this.entries.set(key, new UnreadEntry(0, r.lastReadSeq, r.lastReadSeq));
        this.scheduleRoomCount(r.roomId);
      } else if (r.lastReadSeq > entry.readSeq) {
        this.applyServerRead(r.roomId, r.lastReadSeq);
      }
    }
    // Snapshot keys first: a Map cannot be mutated while iterated.
    for (const key of [...this.entries.keys()]) {
      if (key.startsWith("room:") && !seen.has(key)) this.entries.delete(key);
    }
  }

  /** A room-addressed live envelope (the demux calls this for every room
   *  envelope; deliverLive still handles the transcript for open rooms). A
   *  no-op while viewing (the reader is looking at the rows, and the leave
   *  path asks once at the end); otherwise asks for the count. */
  noteRoomActivity(roomId: string): void {
    const key = roomKey(roomId);
    if (this.viewing.has(key)) return;
    if (!this.entries.has(key)) {
      // An envelope for a room the list has not surfaced yet: the count below
      // is server truth, so the seed only has to exist.
      this.entries.set(key, new UnreadEntry(0, 0, 0));
    }
    this.scheduleRoomCount(roomId);
  }

  /** A 1:1 unread stamp from the server: assign, never derive. The
   *  tagma counts the outbound rows past this user's read cursor, so the number
   *  already accounts for the whole backlog -- a client that syncs a long
   *  history shows one number, not a counter ticking per landed row. Every
   *  carrier lands here: the batch marker's stamp, a live `tagma_unread` frame,
   *  and the keryx's cold-start flush.
   *
   *  While the conversation page is mounted the user is reading it, so the
   *  badge shows 0; the stamped count is still recorded, because a stamp the
   *  tagma computed before our cursor write reached it must not flash a badge
   *  the cursor is about to clear, yet it must also stay the bar the next stamp
   *  has to beat. */
  setTagmaUnread(tagmaId: string, stamp: UnreadStamp): void {
    const { unread } = stamp;
    if (!Number.isFinite(unread) || !Number.isFinite(stamp.cursor)) return;
    const key = tagmaKey(tagmaId);
    let entry = this.entries.get(key);
    if (!entry) {
      entry = new UnreadEntry(0, 0, 0);
      this.entries.set(key, entry);
    }
    // A stamp older than the one already recorded is stale: the carriers
    // (marker, live frame, cold-start flush) can reach this store out of the
    // tagma's emission order. The cursor orders them, and at the same cursor a
    // smaller count is a duplicate of the same fact.
    if (stamp.cursor < entry.stampCursor) return;
    if (stamp.cursor === entry.stampCursor && unread < entry.stampCount) {
      return;
    }
    entry.stampCursor = stamp.cursor;
    entry.stampCount = Math.max(0, Math.floor(unread));
    entry.stampSeen = true;
    entry.count = this.viewing.has(key) ? 0 : entry.stampCount;
  }

  /** The user explicitly opened the conversation (the page's mount effect;
   *  boot auto-open never calls this). Zeroes the badge -- for a 1:1 the
   *  tagma's stamp for the cursor this open reports lands right after and
   *  agrees; rooms schedule the throttled cursor write and stop asking for
   *  counts until the page closes. Safe on a not-yet-listed room: the entry is
   *  zeroed and the leave sync (or a fresher list) fills the watermarks. */
  enter(key: string): void {
    this.viewing.add(key);
    let entry = this.entries.get(key);
    if (!entry) {
      entry = new UnreadEntry(0, 0, 0);
      this.entries.set(key, entry);
    }
    entry.count = 0;
    if (key.startsWith("room:")) {
      this.scheduleRoomPut(key.slice("room:".length), entry.knownSeq);
    }
  }

  /** The room page unmounted: sync knownSeq to the conversation's live
   *  cursor (lines rendered while viewing), flush the cursor write at once
   *  (the throttle window's remainder), and ask for the count AFTER the write
   *  lands -- that answer is what the reading just did, and asking earlier
   *  would badge rows the user has already read. */
  leaveRoom(roomId: string, lastSeq?: number | null): void {
    const key = roomKey(roomId);
    this.viewing.delete(key);
    const entry = this.entries.get(key);
    if (!entry) return;
    if (typeof lastSeq === "number" && lastSeq > entry.knownSeq) {
      entry.knownSeq = lastSeq;
    }
    // A deep-linked room whose history never loaded: trust the server
    // watermark over "seen nothing" so the cursor write does not fall back.
    entry.knownSeq = Math.max(entry.knownSeq, entry.readSeq);
    // readSeq moves only when the write lands (putRoomCursor raises it). Raising
    // it here would claim rows the server never recorded, and the retry path
    // reads a cursor below readSeq as stale.
    void this.flushRoomPut(roomId, entry.knownSeq).then(() => {
      this.scheduleRoomCount(roomId);
    });
  }

  /** The 1:1 chat page unmounted: the viewing flag ends, so the tagma's own
   *  stamped count shows again (whatever it said last, including the count that
   *  arrived while the page masked it). Nothing to flush -- the cursor was
   *  already reported per line while the page was mounted. */
  leaveTagma(tagmaId: string): void {
    const key = tagmaKey(tagmaId);
    this.viewing.delete(key);
    const entry = this.entries.get(key);
    if (entry) entry.count = entry.stampCount;
  }

  /** The room page's line tick while viewing: rendered lines count as read
   *  (the badge stays zero) and the cursor write coalesces into the throttle
   *  window. No count is asked for -- the page owns what has been read, and
   *  the leave path asks once. No-op when not viewing. */
  noteViewedLines(roomId: string, lastSeq: number | null): void {
    if (lastSeq === null || !Number.isSafeInteger(lastSeq)) return;
    const key = roomKey(roomId);
    if (!this.viewing.has(key)) return;
    const entry = this.entries.get(key);
    if (!entry || lastSeq <= entry.knownSeq) return;
    entry.knownSeq = lastSeq;
    this.scheduleRoomPut(roomId, lastSeq);
  }

  /** A room_read_cursor_changed event (or a fresher list watermark): another
   *  session of ours read the room, so the count this store holds is stale.
   *  The watermark only moves forward, and the count is asked for again --
   *  the server owns the arithmetic, and a quiet room has no next envelope to
   *  carry the correction. Masked while viewing: the badge reads 0 there
   *  anyway, and the leave path asks once the reading is done. */
  applyServerRead(roomId: string, readSeqNew: number): void {
    if (!Number.isSafeInteger(readSeqNew) || readSeqNew < 0) return;
    const key = roomKey(roomId);
    const entry = this.entries.get(key);
    if (!entry) {
      // Unknown room (the list has not landed): seed at the server watermark
      // and ask, so the entry starts from truth instead of zero.
      this.entries.set(key, new UnreadEntry(0, readSeqNew, readSeqNew));
      this.scheduleRoomCount(roomId);
      return;
    }
    if (readSeqNew <= entry.readSeq) return; // stale / our own echo
    entry.readSeq = readSeqNew;
    this.scheduleRoomCount(roomId);
  }

  /** The badge count for one conversation key (0 when absent). */
  countOf(key: string): number {
    return this.entries.get(key)?.count ?? 0;
  }

  /** The count as stamped on this device, or undefined when none has ever
   *  reached it. "Unknown" and "zero" answer different questions: a replayed
   *  stamp for a conversation this device already counted must not read as a
   *  new arrival, and only the caller knows which conversation that is.
   *  Reads {@link UnreadEntry.stampCount}: the badge is 0 while the page is
   *  open, and a frame replayed to a mounted page must still be judged
   *  against what this device knew, not against the suppressed badge. An
   *  entry seeded by opening the page has never been told a count, so it
   *  answers undefined until one arrives. */
  countOrUndefined(key: string): number | undefined {
    const entry = this.entries.get(key);
    return entry?.stampSeen ? entry.stampCount : undefined;
  }

  /** Whether the conversation page is explicitly mounted (viewing). The
   *  notification layer reads this to stay silent for the conversation the
   *  user is literally looking at; hidden-window semantics are its own
   *  concern (notify.ts). */
  isViewing(key: string): boolean {
    return this.viewing.has(key);
  }

  /** The bar badge: the unread total across conversations (rendered through
   *  {@link badgeLabel}, so the display caps at "99+"). */
  total(): number {
    let t = 0;
    for (const e of this.entries.values()) t += e.count;
    return t;
  }

  /** Logout: drop all state and pending room writes. Both cursors live
   *  server-side (room_read_cursors, the tagma's chat_read_cursor), so there
   *  is nothing device-local to clear. In-flight count reads re-check the
   *  entry/viewing after each await and bail. */
  reset(): void {
    this.entries.clear();
    this.viewing.clear();
    this.countDirty.clear();
    this.putPending.clear();
    if (this.timerId !== null) {
      clearTimeout(this.timerId);
      this.timerId = null;
      this.timerArmed = false;
    }
  }

  // -- internals -------------------------------------------------------------

  private scheduleRoomCount(roomId: string): void {
    if (this.viewing.has(roomKey(roomId))) return;
    if (this.countInFlight.has(roomId)) {
      this.countDirty.add(roomId); // an event landed mid-flight: one trailing pass
      return;
    }
    void this.runRoomCount(roomId);
  }

  private async runRoomCount(roomId: string): Promise<void> {
    const key = roomKey(roomId);
    this.countInFlight.add(roomId);
    try {
      do {
        this.countDirty.delete(roomId);
        if (!this.entries.has(key) || this.viewing.has(key)) return;
        const stamp = await this.fetchRoomUnread(roomId);
        // The entry may have been reset or dropped mid-await (logout, a
        // re-list that removed the room, the page opening). Re-read after.
        const live = this.entries.get(key);
        if (!live || this.viewing.has(key)) return;
        if (!Number.isFinite(stamp.unread) || !Number.isFinite(stamp.cursor)) {
          return;
        }
        if (stamp.cursor < live.readSeq) {
          // The answer predates a cursor write this store saw land, so its count
          // still holds rows the user has read. Drop it and leave the badge
          // where it is: the next envelope, cursor echo or list asks again.
          return;
        }
        live.count = Math.max(0, Math.floor(stamp.unread));
        live.readSeq = Math.max(live.readSeq, stamp.cursor);
      } while (this.countDirty.has(roomId) && !this.viewing.has(key));
    } catch {
      // Read failure: the entry stays as it was, and the next envelope, cursor
      // echo or list refresh asks again. A dropped answer delays the badge,
      // never fakes it.
    } finally {
      this.countInFlight.delete(roomId);
    }
  }

  private scheduleRoomPut(roomId: string, seq: number): void {
    const pending = this.putPending.get(roomId);
    if (pending !== undefined && seq <= pending) return;
    this.putPending.set(roomId, seq);
    if (this.timerArmed) return; // window open: coalesce
    this.timerArmed = true;
    this.timerId = this.armTimer(() => void this.drainRoomPuts());
  }

  private async drainRoomPuts(): Promise<void> {
    this.timerArmed = false;
    this.timerId = null;
    const batch = [...this.putPending.entries()];
    this.putPending.clear();
    for (const [roomId, seq] of batch) {
      await this.putRoomCursor(roomId, seq);
    }
  }

  private async putRoomCursor(roomId: string, seq: number): Promise<void> {
    // Client-side defense: only an observed, safe, non-negative integer is
    // ever sent. The server clamps to max, so a late lower write cannot
    // regress the cursor -- it is a no-op, not a hazard.
    if (!Number.isSafeInteger(seq) || seq < 0) return;
    try {
      await this.putCursor(roomId, seq);
      const entry = this.entries.get(roomKey(roomId));
      if (entry) entry.readSeq = Math.max(entry.readSeq, seq);
    } catch {
      // Write failure: the cursor lags; the next tick/leave retries.
    }
  }

  /** Flush one room's cursor write at once (leave), cancelling any pending
   *  throttled write for it; the window timer stays for other rooms. The
   *  returned promise settles when the write does (the caller asks for the
   *  count behind it). */
  private flushRoomPut(roomId: string, seq: number): Promise<void> {
    this.putPending.delete(roomId);
    const done = this.putRoomCursor(roomId, seq);
    if (this.putPending.size === 0 && this.timerId !== null) {
      clearTimeout(this.timerId);
      this.timerId = null;
      this.timerArmed = false;
    }
    return done;
  }
}

export const unreadStore = new UnreadStore();
