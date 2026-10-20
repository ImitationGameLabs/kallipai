// UnreadStore: the per-conversation unread-badge state.
// One entry per conversation key -- `room:{room_id}` for multi-member rooms,
// `tagma:{tagma_id}` for bilateral 1:1 chats -- tracking:
//
//   count      the number the badge shows (rendered through {@link badgeLabel},
//              which caps the display at "99+").
//   knownSeq   rooms only: the highest line seq this session has SEEN from any
//              source (SSE live frame, catch-up page, room pull). The dedup
//              fence: any line at or below it is a replay and is ignored,
//              whatever path carried it -- so live frames racing a catch-up
//              pull can never double-count.
//   readSeq    rooms only: the server-acknowledged read cursor (the
//              room_read_cursors watermark).
//   unreadFrom rooms only: the cursor position the current count was computed
//              against (a cursor-changed event reduces the count by exactly
//              the watermark advance, so an out-of-order or partial event can
//              neither resurrect unread nor push the count negative).
//
// 1:1 keeps no local tally at all: the tagma owns the number. It counts the
// outbound rows past this user's read cursor in its own store and stamps the
// result -- when it appends a reply and when the cursor moves -- through
// three carriers:
//   - the `history_batch_end` marker's `unread` (every catch-up window and
//     before-page answers with the count as of that answer);
//   - a live relay frame the keryx surfaces as `tagma_unread` on this user's
//     own app stream;
//   - the keryx's cached copy, flushed on `/me/events` connect, so a cold
//     client badges before its first conversation opens.
// The store only ASSIGNS those numbers ({@link setTagmaUnread}): a device
// syncing a long history shows one number, never a counter ticking per
// landed row.
//
// The other direction is the read cursor: the client reports how far it has
// read and the tagma stamps the resulting count (again through
// {@link setTagmaUnread}). Reporting is plumbing, not counting --
// RelayConversation sends the id of the newest row it renders, on page mount
// and per live line while the page stays mounted; the store reads back only
// what comes stamped.
//
// Cold start is the third carrier's job: the keryx keeps one cached
// snapshot per tunnel and the tagma re-announces every partition as its
// upstream link comes up, so `/me/events` opens with one frame per tagma.
// The hub's badge is then a sum of assigned numbers -- no local
// accumulation and no per-conversation open required.
//
// Metadata note: a plaintext upstream carrying `cursor`/`unread` lets the
// relay observe how far this user has read in a 1:1 -- the same trade the
// room cursor already makes (room watermarks are server-side too).
// Deliberate, and scoped: those frames reach the user's own app stream
// only, never the peer tagma or any other principal.
//
// Rooms still derive their count locally, because the lesche has no
// per-room stamp:
//   - rooms: the RootLayout envelope demux calls {@link noteRoomActivity} for
//     every room-addressed envelope, open room or not. A live room envelope
//     carries no server seq (the fan reuses the sender's envelope; the route
//     ignores sequence_n), so precise counting needs a seq-bearing pull:
//     noteRoomActivity marks the room dirty and single-flights one page after
//     knownSeq (CATCHUP_PAGE mirrors the lesche's HISTORY_MAX_LIMIT) -- a
//     full page reads as "99+", and knownSeq advances to the page tail. A
//     landing mid-pull leaves a dirty flag for one trailing re-pull, so a
//     burst can hide rows.
//
// Viewing discipline: only an explicitly opened conversation page
// is "viewing" -- boot auto-open never calls enter(), so a login can never
// silently clear badges. Scope note: viewing keys on the page being mounted,
// not on document visibility; a background tab with the room page open still
// counts as reading it (the notification layer -- notify.ts -- owns hidden-
// window semantics).
//
// While viewing, the room page's line tick ({@link noteViewedLines}) advances
// knownSeq and coalesces the cursor write into one request per
// PUT_THROTTLE_MS window (a busy chat must not POST per line); the leave path
// flushes once. The client-side defense lives in
// putRoomCursor: only a safe non-negative integer the client actually
// observed is ever sent -- the store never invents a value the server's
// max-clamp would pin irreversibly (and a late lower write cannot regress the
// cursor anyway: the server clamps to max).
import { SvelteMap } from "svelte/reactivity";
import { archeionSession, lescheClientOrFail } from "./archeion.svelte.ts";

/** Badge display cap: anything above renders as "99+" ({@link badgeLabel}). */
export const UNREAD_CAP = 99;
/** One room catch-up page. Mirrors the lesche's HISTORY_MAX_LIMIT
 *  (routes/rooms.rs) so a full page is one request; a full page is the "99+"
 *  signal, never a correctness input. */
export const CATCHUP_PAGE = 200;
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

/** One unread stamp as the tagma sends it: the count and the read cursor it
 *  was measured against. One object because the pair is the ordering fact -- a
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
  unreadFrom = $state(0);
  /** 1:1 only: the read cursor the current count was stamped against. A
   *  stamp computed before a cursor write landed carries a smaller cursor
   *  than one computed after it, so the pair orders the stamps even
   *  when two carriers race on their way here. */
  stampCursor = 0;
  /** 1:1 only: the tagma's own number, never zeroed by viewing. {@link count}
   *  is what the badge shows (0 while the page is mounted); this is the fact
   *  the next stamp has to beat, so opening the page cannot lower the bar a
   *  later stamp has to clear. */
  stampCount = 0;

  constructor(
    count: number,
    knownSeq: number,
    readSeq: number,
    unreadFrom: number,
  ) {
    this.count = count;
    this.knownSeq = knownSeq;
    this.readSeq = readSeq;
    this.unreadFrom = unreadFrom;
  }
}

/** One seq-bearing row of a room catch-up pull (the store only needs the seq
 *  and the sender identity -- "is this mine" -- for the count). */
export interface RoomCatchupRow {
  seq: number;
  senderId: string;
}

type RoomPageFetcher = (
  roomId: string,
  afterSeq: number,
  limit: number,
) => Promise<RoomCatchupRow[]>;
type ReadCursorPutter = (roomId: string, seq: number) => Promise<void>;
type SelfIdGetter = () => string | null;
type TimerArmer = (fn: () => void) => ReturnType<typeof setTimeout>;

function defaultFetchRoomPage(
  roomId: string,
  afterSeq: number,
  limit: number,
): Promise<RoomCatchupRow[]> {
  return lescheClientOrFail()
    .fetchRoomMessages(roomId, { afterSeq, limit })
    .then((rows) => rows.map((r) => ({ seq: r.seq, senderId: r.sender.id })));
}
function defaultPutCursor(roomId: string, seq: number): Promise<void> {
  return lescheClientOrFail().setRoomReadCursor(roomId, seq);
}
function defaultSelfId(): string | null {
  return archeionSession.participantId;
}
function defaultArmTimer(fn: () => void): ReturnType<typeof setTimeout> {
  return setTimeout(fn, PUT_THROTTLE_MS);
}

class UnreadStore {
  private entries = new SvelteMap<string, UnreadEntry>();
  /** Keys whose conversation page is explicitly mounted (viewing). */
  private viewing = new Set<string>();
  // Room pull scheduler: single-flight with a dirty trailing re-pull.
  private pullInFlight = new Set<string>();
  private pullDirty = new Set<string>();
  // Room cursor-write throttle: pending writes by room + the window timer.
  private putPending = new Map<string, number>();
  private timerArmed = false;
  private timerId: ReturnType<typeof setTimeout> | null = null;
  // Test seams (null restores the production default).
  private fetchRoomPage: RoomPageFetcher = defaultFetchRoomPage;
  private putCursor: ReadCursorPutter = defaultPutCursor;
  private selfId: SelfIdGetter = defaultSelfId;
  private armTimer: TimerArmer = defaultArmTimer;

  setRoomPageFetcher(f: RoomPageFetcher | null): void {
    this.fetchRoomPage = f ?? defaultFetchRoomPage;
  }
  setReadCursorPutter(f: ReadCursorPutter | null): void {
    this.putCursor = f ?? defaultPutCursor;
  }
  setSelfId(f: SelfIdGetter | null): void {
    this.selfId = f ?? defaultSelfId;
  }
  setTimerArmer(f: TimerArmer | null): void {
    this.armTimer = f ?? defaultArmTimer;
  }

  /** Seed room entries from the registry list (the list_rooms response
   *  carries the caller-scoped last_read_seq -- the zero-extra-request
   *  initialization). First sight seeds the watermarks and schedules the
   *  counting pull; a re-list only advances a server watermark that another
   *  session moved (the list is the resync ground truth). Rooms gone from
   *  the registry drop their entry (member removal / room deletion). */
  initRooms(rows: { roomId: string; lastReadSeq: number }[]): void {
    const seen = new Set<string>();
    for (const r of rows) {
      const key = roomKey(r.roomId);
      seen.add(key);
      const entry = this.entries.get(key);
      if (!entry) {
        this.entries.set(
          key,
          new UnreadEntry(0, r.lastReadSeq, r.lastReadSeq, r.lastReadSeq),
        );
        this.scheduleRoomPull(r.roomId);
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
   *  no-op while viewing (the open page owns counting + the cursor);
   *  otherwise schedules the precise seq-bearing pull. */
  noteRoomActivity(roomId: string): void {
    const key = roomKey(roomId);
    if (this.viewing.has(key)) return;
    if (!this.entries.has(key)) {
      // Envelope for a room the list has not surfaced yet: seeded from the
      // pull (server truth), not zero.
      this.entries.set(key, new UnreadEntry(0, 0, 0, 0));
    }
    this.scheduleRoomPull(roomId);
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
      entry = new UnreadEntry(0, 0, 0, 0);
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
    entry.count = this.viewing.has(key) ? 0 : entry.stampCount;
  }

  /** The user explicitly opened the conversation (the page's mount effect;
   *  boot auto-open never calls this). Zeroes the badge -- for a 1:1 the
   *  tagma's stamp for the cursor this open reports lands right after and
   *  agrees; rooms schedule the throttled cursor write. Safe on a
   *  not-yet-listed room: the entry is zeroed and the leave sync (or a
   *  fresher list) fills the watermarks. */
  enter(key: string): void {
    this.viewing.add(key);
    let entry = this.entries.get(key);
    if (!entry) {
      entry = new UnreadEntry(0, 0, 0, 0);
      this.entries.set(key, entry);
    }
    entry.count = 0;
    entry.unreadFrom = Math.max(entry.unreadFrom, entry.knownSeq);
    if (key.startsWith("room:")) {
      this.scheduleRoomPut(key.slice("room:".length), entry.knownSeq);
    }
  }

  /** The room page unmounted: sync knownSeq to the conversation's live
   *  cursor (lines rendered while viewing), then flush the cursor write at
   *  once (the throttle window's remainder). */
  leaveRoom(roomId: string, lastSeq?: number | null): void {
    const key = roomKey(roomId);
    this.viewing.delete(key);
    const entry = this.entries.get(key);
    if (!entry) return;
    if (typeof lastSeq === "number" && lastSeq > entry.knownSeq) {
      entry.knownSeq = lastSeq;
    }
    // A deep-linked room whose history never loaded: trust the server
    // watermark over "seen nothing" so the next pull does not recount.
    entry.knownSeq = Math.max(entry.knownSeq, entry.readSeq);
    entry.readSeq = Math.max(entry.readSeq, entry.knownSeq);
    entry.unreadFrom = Math.max(entry.unreadFrom, entry.knownSeq);
    this.flushRoomPut(roomId, entry.knownSeq);
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
   *  window. No-op when not viewing (the catch-up pull owns counting then). */
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
   *  session of ours read the room. Reduce the count by exactly the watermark
   *  advance; never regress (a stale event is dropped on readSeq). */
  applyServerRead(roomId: string, readSeqNew: number): void {
    if (!Number.isSafeInteger(readSeqNew) || readSeqNew < 0) return;
    const key = roomKey(roomId);
    const entry = this.entries.get(key);
    if (!entry) {
      // Unknown room (list not landed): seed at the server watermark so a
      // later pull does not recount rows read elsewhere.
      this.entries.set(
        key,
        new UnreadEntry(0, readSeqNew, readSeqNew, readSeqNew),
      );
      return;
    }
    if (readSeqNew <= entry.readSeq) return; // stale / our own echo
    const advance = readSeqNew - entry.unreadFrom;
    if (advance > 0) {
      entry.count = Math.max(0, entry.count - Math.min(entry.count, advance));
    }
    entry.readSeq = readSeqNew;
    entry.unreadFrom = Math.max(entry.unreadFrom, readSeqNew);
  }

  /** The badge count for one conversation key (0 when absent). */
  countOf(key: string): number {
    return this.entries.get(key)?.count ?? 0;
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
   *  is nothing device-local to clear. In-flight pulls re-check the
   *  entry/viewing after each await and bail. */
  reset(): void {
    this.entries.clear();
    this.viewing.clear();
    this.pullDirty.clear();
    this.putPending.clear();
    if (this.timerId !== null) {
      clearTimeout(this.timerId);
      this.timerId = null;
      this.timerArmed = false;
    }
  }

  // -- internals -------------------------------------------------------------

  private scheduleRoomPull(roomId: string): void {
    if (this.pullInFlight.has(roomId)) {
      this.pullDirty.add(roomId); // mail landed mid-pull: one trailing pass
      return;
    }
    void this.runRoomPull(roomId);
  }

  private async runRoomPull(roomId: string): Promise<void> {
    const key = roomKey(roomId);
    if (!this.entries.has(key) || this.viewing.has(key)) return;
    this.pullInFlight.add(roomId);
    try {
      do {
        this.pullDirty.delete(roomId);
        const entry = this.entries.get(key);
        if (!entry || this.viewing.has(key)) return;
        const rows = await this.fetchRoomPage(
          roomId,
          entry.knownSeq,
          CATCHUP_PAGE,
        );
        // The entry may have been reset/dropped mid-await (logout, re-list,
        // the room page opening). Re-read after the await.
        const live = this.entries.get(key);
        if (!live || this.viewing.has(key)) return;
        for (const row of rows) {
          if (row.seq <= live.knownSeq) continue; // dedup fence
          live.knownSeq = row.seq;
          // Read on another session: the watermark covers it (knownSeq still
          // advances so the fence holds).
          if (row.seq <= live.readSeq) continue;
          if (row.senderId !== this.selfId()) live.count += 1;
        }
      } while (this.pullDirty.has(roomId) && !this.viewing.has(key));
    } catch {
      // Fetch failure: the entry stays as-is; the next envelope (or list
      // refresh) retries. A dropped pull delays the badge, never fakes it.
    } finally {
      this.pullInFlight.delete(roomId);
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
    // Client-side defense: only an observed, safe,
    // non-negative integer is ever sent. The server clamps to max, so a late
    // lower write cannot regress the cursor -- it is a no-op, not a hazard.
    if (!Number.isSafeInteger(seq) || seq < 0) return;
    try {
      await this.putCursor(roomId, seq);
      const entry = this.entries.get(roomKey(roomId));
      if (entry) {
        entry.readSeq = Math.max(entry.readSeq, seq);
        entry.unreadFrom = Math.max(entry.unreadFrom, seq);
      }
    } catch {
      // Write failure: the cursor lags; the next tick/leave retries. The
      // badge is local truth and unaffected.
    }
  }

  /** Flush one room's cursor write at once (leave), cancelling any pending
   *  throttled write for it; the window timer stays for other rooms. */
  private flushRoomPut(roomId: string, seq: number): void {
    this.putPending.delete(roomId);
    void this.putRoomCursor(roomId, seq);
    if (this.putPending.size === 0 && this.timerId !== null) {
      clearTimeout(this.timerId);
      this.timerId = null;
      this.timerArmed = false;
    }
  }
}

export const unreadStore = new UnreadStore();
