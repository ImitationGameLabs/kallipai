/// <reference lib="dom" />

import type { FileAttachment } from "@kallipai/kallipai-common";
// The per-device IndexedDB cache of already-loaded chat lines: a durable
// mirror of what the app has rendered, so a refresh/reopen restores the
// conversation from local state and only asks the tagma for the rows after the
// newest position it rendered (`after_seq`), instead of re-pulling the window.
//
// This is a DUMB key-value store of `{conversationId, historyId, role, text}`
// tuples. It deliberately does NOT interpret `role` (a UI concept owned by
// kallipai-ui's transcript reducer) — the UI writes tuples it extracted via its
// own `cacheLineOf`, and reads them back as-is. Keeping the semantics out of
// this layer means this package never depends on the UI's role vocabulary.
//
// No key carries a secret; the cache is plaintext, consistent with the
// host/device trust model (the tagma's SQLite store is plaintext too). Logout
// clears it.

// --- open contract ----------------------------------------------------------
//
// The cache is derived data: a failure here degrades the page to a direct
// re-pull and never gates it. Three rules keep that true.
//
//  1. One open per document, single flight. `db()` memoizes the connection
//     and every reader joins that one request, so two of them never race
//     for the upgrade lock.
//  2. The open runs under ONE budget. An open that another tab blocks, or
//     one that never settles, rejects instead of waiting: waiting is what
//     left every open tab staring at an empty history after a version
//     upgrade.
//  3. Opening needs a permit. `prepareCache()` is the only grant on the
//     ordinary path (attach points and an explicit retry); message-store
//     reads and writes never open on their own, so a blocked open cannot be
//     retried once per arriving frame. The pending store is the one
//     exception: those rows exist nowhere else, so a send may ask for a
//     permit itself (see `durableDb`). A failure -- open, epoch check or
//     read -- starts a cooldown that no automatic path crosses; a user's
//     explicit retry lifts it (`permitRetry()`), and the permit, the budget
//     and single flight still bound the attempt that follows.
//
// A connection that arrives after its promise settled -- budget spent, or an
// open reported blocked that completed later -- is closed on arrival: leaving
// it open puts a version block in front of the next page load.

/** One cached content line. `historyId` is the tagma `chat_history.id`; the
 * (conversationId, historyId) pair is the IndexedDB primary key. `role` is an
 * opaque string the UI (which owns the role vocabulary) writes and reads back
 * -- this layer does not interpret it. `text` is the rendered line. `sender`
 * (when present) is the UI-facing sender the UI wrote, stored verbatim so the
 * cache-hydrate path renders the author without a server round-trip.
 * `createdAt` is the RFC 3339 send time; absent on rows cached before the field
 * existed (reads tolerate `undefined`). */
export interface CachedLine {
  readonly conversationId: string;
  readonly historyId: number;
  readonly role: string;
  readonly text: string;
  readonly sender?: {
    readonly kind: "user" | "agent";
    readonly id: string;
    readonly handle: string;
  };
  readonly createdAt?: string;
  /** The message's file attachment, when the sender shared one; absent on
   *  rows cached before the field existed (reads tolerate `undefined`). */
  readonly attachment?: FileAttachment;
  /** The 1:1 history row id this message quotes, when it quotes one;
   *  absent on rows cached before the field existed. Kept so a refresh
   *  replays the reference instead of losing it (the row's own copy wins
   *  on a merge, so a stale value cannot overwrite a fresher replay). */
  readonly quote?: number;
  /** The row's position in its conversation's log (`chat_history.seq`): the unit
   *  the resume cursor and the read watermark are expressed in. Absent when the
   *  tagma did not stamp the row (a frame with no position), which leaves the
   *  cursor where it was. */
  readonly seq?: number;
}

const DB_NAME = "kallipai-keryx";
// v2: the keyPath field was renamed `convId` -> `conversationId`. v3: the row
// gained an optional `sender`. The messages store is a disposable derived
// cache (re-pulled from the tagma), so on upgrade we drop+recreate it
// rather than migrate rows. The pending store holds unsent sends and is
// created once and never dropped. v6: the 1:1 read watermarks live in the
// tagma, so the browser's copy is deleted on upgrade -- an old install would
// otherwise keep rows nothing reads. v7: rows gained the `seq` position, so
// the store is dropped once more -- a row without a position cannot feed the
// resume cursor.
// v8: rows are stamped with the numbering they were read under, in a `meta`
// store; rows from another numbering are cleared on open, in the same
// transaction that writes the stamp.
const DB_VERSION = 8;
const STORE = "messages";
const PENDING_STORE = "pending";
const META_STORE = "meta";
/** The one key of META_STORE: the history numbering the rows were read
 *  under. */
const EPOCH_KEY = "historySchema";
/** The 1:1 watermark store that v4 added and v6 drops: one row per
 *  conversation. */
const V5_WATERMARK_STORE = "read_watermarks";

let dbPromise: Promise<IDBDatabase> | null = null;

/** The cache could not be opened: blocked by another tab, budget spent, or no
 *  permit. Every caller degrades to a direct re-pull; it never reaches the
 *  user as an error. */
export class CacheUnavailable extends Error {
  constructor(message = "cache unavailable") {
    super(message);
    this.name = "CacheUnavailable";
  }
}

const OPEN_BUDGET_MS = 1000;
const COOLDOWN_MS = 10_000;

/** Real timers; tests shrink them through the setters at the end of file. */
let openBudgetMs = OPEN_BUDGET_MS;
let cooldownMs = COOLDOWN_MS;

/** Granted by prepareCache(), spent by the next open attempt. */
let openPermit = false;
/** No open may start before this wall-clock time (0 = open). */
let cooldownUntil = 0;
/** The in-flight open request, shared by every caller (see `openDb`). */
let openRequest: Promise<IDBDatabase> | null = null;
/** The attempt that started the in-flight open already gave up. */
let openAbandoned = false;

/** The numbering the cached rows are known to be valid under, once a check
 *  has completed for this connection. Null until the first check. */
let preparedSchema: number | null = null;
/** The epoch check in flight, so two attaches with the same numbering share
 *  one transaction instead of queueing two. */
let epochPending: Promise<void> | null = null;

/** The one open request of this document: the memo `db()` builds on,
 * released when the request settles so a later attempt can retry. */
function openDb(): Promise<IDBDatabase> {
  if (openRequest) return openRequest;
  const attempt = openOnce();
  openRequest = attempt;
  const release = () => {
    if (openRequest === attempt) openRequest = null;
  };
  attempt.then(release, release);
  return attempt;
}

/** Lazily open (and upgrade-create) the `kallipai-keryx` DB. A blocked open
 * rejects at once: another tab holding the older version open is not a wait
 * that ends by itself, so waiting only hides the page behind it. */
function openOnce(): Promise<IDBDatabase> {
  return new Promise<IDBDatabase>((resolve, reject) => {
    if (typeof indexedDB === "undefined") {
      reject(new CacheUnavailable("IndexedDB unavailable"));
      return;
    }
    let settled = false;
    const settle = (act: () => void) => {
      if (settled) return;
      settled = true;
      act();
    };
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = (event) => {
      const d = req.result;
      // Drop any older store (older keyPath, row shape, or rows without the
      // `seq` position) and recreate. The cache is disposable and re-pulled
      // from the tagma on the next open.
      if (event.oldVersion < 7 && d.objectStoreNames.contains(STORE)) {
        d.deleteObjectStore(STORE);
      }
      if (!d.objectStoreNames.contains(STORE)) {
        // Composite keyPath [conversationId, historyId] is itself ordered, so a
        // key range on the leading `conversationId` element selects a
        // conversation and returns rows in historyId order — no extra index.
        d.createObjectStore(STORE, {
          keyPath: ["conversationId", "historyId"],
        });
      }
      // v6: delete the watermark store, and only it -- the pending rows
      // beside it are unsent messages that exist nowhere else, so the rest
      // of the database is left exactly as it is.
      if (d.objectStoreNames.contains(V5_WATERMARK_STORE)) {
        d.deleteObjectStore(V5_WATERMARK_STORE);
      }
      if (!d.objectStoreNames.contains(PENDING_STORE)) {
        // v5: unsent local messages, one row per attempt, keyed
        // [tagmaId, localSeq]. These rows exist nowhere else -- a dropped
        // row is a lost message.
        d.createObjectStore(PENDING_STORE, {
          keyPath: ["tagmaId", "localSeq"],
        });
      }
      if (!d.objectStoreNames.contains(META_STORE)) {
        // v8: facts about the cache itself, one key each. No keyPath: the
        // next fact is a new key, not another version.
        d.createObjectStore(META_STORE);
      }
    };
    req.onsuccess = () => {
      const d = req.result;
      // Late arrival: the budget was spent or the open was reported
      // blocked. Closing it is the whole point -- an unclosed connection
      // would block the NEXT page load's upgrade.
      if (settled || openAbandoned) {
        d.close();
        return;
      }
      settle(() => {
        // Yield the version lock when another tab wants to upgrade: this
        // page then rides the cache-less path until an explicit retry.
        d.onversionchange = () => {
          d.close();
          if (dbPromise) dbPromise = null;
          preparedSchema = null;
        };
        resolve(d);
      });
    };
    req.onblocked = () =>
      settle(() =>
        reject(new CacheUnavailable("cache open blocked by another tab")),
      );
    req.onerror = () =>
      settle(() => reject(req.error ?? new CacheUnavailable("open failed")));
  });
}

/** The connection, or a rejection: no permit, cooling down, or the attempt
 * failed. Attempts start only on a permit, which this call spends. */
function db(): Promise<IDBDatabase> {
  if (dbPromise) {
    openPermit = false;
    return dbPromise;
  }
  if (!openPermit) {
    return Promise.reject(new CacheUnavailable("no open permit"));
  }
  if (Date.now() < cooldownUntil) {
    openPermit = false;
    return Promise.reject(new CacheUnavailable("cache is cooling down"));
  }
  openPermit = false;
  openAbandoned = false;
  // The attempt's own flag, not the shared one: a connection that lands
  // after its promise settled is closed by this chain (see openOnce).
  let abandoned = false;
  const attempt = withinBudget(
    openDb().then((connection) => {
      if (abandoned) {
        connection.close();
        throw new CacheUnavailable("cache open abandoned");
      }
      return connection;
    }),
  );
  dbPromise = attempt;
  attempt.then(
    () => undefined,
    () => {
      abandoned = true;
      if (dbPromise === attempt) cacheFailed();
    },
  );
  return attempt;
}

/** The one failure response: forget the connection, spend any permit, and
 *  touch nothing here for a cooldown. Every failing leg lands here -- a
 *  blocked or over-budget open, a refused epoch transaction -- so "a broken
 *  cache goes quiet" is one rule instead of one per path. */
function cacheFailed(connection?: IDBDatabase): void {
  if (connection) {
    try {
      connection.close();
    } catch {
      // A connection already closing needs nothing from us.
    }
  }
  dbPromise = null;
  preparedSchema = null;
  openPermit = false;
  cooldownUntil = Date.now() + cooldownMs;
  // A budget that expired over a still-pending request abandons it; a
  // connection arriving later is closed (see openOnce).
  if (openRequest) openAbandoned = true;
}

/** Reject after `openBudgetMs` unless `work` settled first. The budget
 * covers the open the caller started, not one request inside it: an open
 * can wait on another tab. */
function withinBudget<T>(work: Promise<T>): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new CacheUnavailable("cache open exceeded its budget")),
      openBudgetMs,
    );
    work.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

/** An open for the durable pending store; why it may take a permit of
 * its own is in the open contract at the top of the file. */
function durableDb(): Promise<IDBDatabase> {
  openPermit = true;
  return db();
}

/** Grant one open attempt and wait for it: the usual way an open starts.
 * Call it before the first cache read of an attach, and again when the user
 * retries, so opening before the first read is ordinary code order rather
 * than an import-order convention. `durableDb` is the other entry, for a
 * pending line that exists nowhere else. Resolves when the connection is
 * usable; rejects (CacheUnavailable) when it is not -- callers degrade. */
export async function prepareCache(historySchema?: number): Promise<void> {
  if (!dbPromise) {
    openPermit = true;
    await db();
  }
  // Without a numbering there is nothing to compare against, and a clear is
  // worse than a stale row: a caller that never passed one keeps the cache it
  // has (null is not a "clear everything" default).
  if (historySchema !== undefined) await checkEpoch(historySchema);
}

/** A user's explicit retry: it releases the cooldown, because a click is the
 *  strongest statement that this page should try again. The bounds that
 *  matter stay elsewhere -- one attempt per call, single flight, and a
 *  budget per attempt -- so a retry loosens nothing else. */
export function permitRetry(): void {
  cooldownUntil = 0;
}

/** Mark the rows as read under `historySchema`, retiring the ones read under
 *  another. A numbering that changed means row ids moved, so every cached row
 *  is a position the tagma may hand to something else; the cache is derived
 *  data and is re-pulled, so dropping it is the cheap side of the trade. */
async function checkEpoch(historySchema: number): Promise<void> {
  if (preparedSchema === historySchema) return;
  if (epochPending !== null) {
    await epochPending;
    if (preparedSchema === historySchema) return;
  }
  let connection: IDBDatabase | undefined;
  const attempt = (async () => {
    const d = await db();
    connection = d;
    await epochTransaction(d, historySchema);
    preparedSchema = historySchema;
  })();
  epochPending = attempt;
  try {
    await attempt;
  } catch (error) {
    // A refused transaction is a failed cache, not a bad row: the check
    // cools down like a failed open, so the retry a catch-up would make
    // cannot reopen it once per arrival.
    cacheFailed(connection);
    throw error;
  } finally {
    if (epochPending === attempt) epochPending = null;
  }
}

/** The read, the clear and the new stamp ride ONE transaction: a clear that
 *  committed without its stamp would run again on every open, and a stamp
 *  without its clear would leave rows the next read cannot place. */
function epochTransaction(
  d: IDBDatabase,
  historySchema: number,
): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const tx = d.transaction([META_STORE, STORE], "readwrite");
    const meta = tx.objectStore(META_STORE);
    const read = meta.get(EPOCH_KEY);
    read.onsuccess = () => {
      if (read.result === historySchema) return;
      tx.objectStore(STORE).clear();
      meta.put(historySchema, EPOCH_KEY);
    };
    tx.oncomplete = () => resolve();
    // Every failure leaves through CacheUnavailable: the cache contract is
    // "rejects, so the caller degrades", and a raw DOMException would put a
    // second error shape on that path.
    const failed = (what: string) => {
      const error = new CacheUnavailable("cache epoch check " + what);
      if (tx.error !== null) Object.assign(error, { cause: tx.error });
      reject(error);
    };
    tx.onerror = () => failed("failed");
    tx.onabort = () => failed("aborted");
  });
}

/** A key range covering every (conversationId, *) row for one conversation. The
 * composite keyPath orders by conversationId first, so bounding it on the
 * leading element selects exactly the conversation, in historyId order.
 *
 * The lower bound is `[conversationId, 1]`, not `[conversationId, -Infinity]`:
 * historyId is the tagma's AUTOINCREMENT `chat_history.id`, whose minimum is 1,
 * and `-Infinity` as a compound-key element throws `DataError` on some
 * Safari/older-Firefox engines (which would otherwise silently disable the
 * cache via the surrounding try/catch and force a full re-pull on every open).
 */
function conversationRange(conversationId: string): IDBKeyRange {
  return IDBKeyRange.bound(
    [conversationId, 1],
    [conversationId, Number.MAX_SAFE_INTEGER],
  );
}

/** Run a single request on one store of an open connection. */
function requestOn<T>(
  d: IDBDatabase,
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest<T>,
  store: string,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const req = fn(d.transaction(store, mode).objectStore(store));
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("idb request failed"));
  });
}

/** Run a single request on one store (default: the messages cache). */
function run<T>(
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest<T>,
  store: string = STORE,
): Promise<T> {
  return db().then((d) => requestOn(d, mode, fn, store));
}

/** As run(), for the durable pending store: that store may open on its own. */
function runDurable<T>(
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest<T>,
): Promise<T> {
  return durableDb().then((d) => requestOn(d, mode, fn, PENDING_STORE));
}

/** Collect up to `k` rows scanning `range` NEWEST-first with a cursor, then
 *  return them oldest-first. The composite keyPath orders rows by
 *  historyId within the conversation, so a "prev" cursor walks ids
 *  descending. Returns [] on any cache error — the same
 *  degrade-to-refetch contract as the hydrate paths. */
async function tailScan(range: IDBKeyRange, k: number): Promise<CachedLine[]> {
  try {
    const rows = await db().then(
      (d) =>
        new Promise<CachedLine[]>((resolve, reject) => {
          const out: CachedLine[] = [];
          const req = d
            .transaction(STORE, "readonly")
            .objectStore(STORE)
            .openCursor(range, "prev");
          req.onsuccess = () => {
            const c = req.result;
            if (c && out.length < k) {
              out.push(c.value as CachedLine);
              c.continue();
            } else {
              resolve(out);
            }
          };
          req.onerror = () =>
            reject(req.error ?? new Error("idb cursor failed"));
        }),
    );
    return rows.reverse();
  } catch {
    return [];
  }
}

/** Load the most recent `k` cached lines for a conversation, oldest-first.
 *  The lazy-window hydrate: enough to fill a screen without materializing
 *  the whole cached history. */
export async function readTail(
  conversationId: string,
  k: number,
): Promise<CachedLine[]> {
  try {
    return await tailScan(conversationRange(conversationId), k);
  } catch {
    return []; // no/failed IndexedDB: degrade to refetch
  }
}

/** Load up to `k` cached lines strictly older than `beforeId`,
 *  oldest-first. The scroll-to-top page source: everything already
 *  loaded sits at ids >= beforeId, so the upper bound is exclusive
 *  (matching the server read_before semantics). The [conversationId, 1]
 *  lower bound leans on the same invariant as conversationRange:
 *  historyId is the tagma's AUTOINCREMENT chat_history.id, whose
 *  minimum is 1 — no row can sit below it. */
export async function readTailBefore(
  conversationId: string,
  beforeId: number,
  k: number,
): Promise<CachedLine[]> {
  try {
    return await tailScan(
      IDBKeyRange.bound(
        [conversationId, 1],
        [conversationId, beforeId],
        false,
        true,
      ),
      k,
    );
  } catch {
    return []; // no/failed IndexedDB: degrade to refetch
  }
}

/** Put (insert or replace) one cached line. Swallow errors: a failed write
 * only means the next reopen re-pulls that row from the tagma. */
export async function put(line: CachedLine): Promise<void> {
  try {
    await run("readwrite", (s) => s.put(line));
  } catch {
    // best-effort cache; a write failure is non-fatal
  }
}

/** Drop every cached line for a conversation (logout / reset). */
export async function clear(conversationId: string): Promise<void> {
  try {
    await run("readwrite", (s) => s.delete(conversationRange(conversationId)));
  } catch {
    // best-effort
  }
}
// --- unsent local messages (v5) ---------------------------------------------

/** One locally-sent-but-unconfirmed chat line, durable across reloads. These
 * rows exist nowhere else -- the tagma has no copy until the send lands -- so
 * the store is durable (never dropped on upgrade) and the row carries the
 * rendered text, not a CachedLine view. The localSeq is a negative monotonic
 * stamp that doubles as the transcript's synthetic line id, so the pending
 * row and its optimistic bubble share one key for retry/confirm/remove. */
export interface PendingLine {
  readonly tagmaId: string;
  readonly localSeq: number;
  readonly text: string;
  readonly attachment?: FileAttachment;
  /** The row id this line quotes, when it is a quote (1:1 history id). An id
   * only: the quoted text is resolved from history, never persisted here. */
  readonly quote?: number;
  readonly createdAt: string;
  readonly lastError?: string;
  /** Lifecycle of the row: "queued" = never accepted by the server (the
   * reconnect flush may send it); "failed" = a send attempt failed and the
   * user must retry explicitly (never auto-resent). Optional for rows
   * written before the field existed: an absent status with a lastError is
   * read as failed, without one as queued. */
  readonly status?: "queued" | "failed";
}

/** Persist one unsent line (best-effort: a failed write only means the line
 * is lost on reload, the same degrade the in-memory queue already has). */
export async function putPending(row: PendingLine): Promise<void> {
  try {
    await runDurable("readwrite", (s) => s.put(row));
  } catch {
    // best-effort
  }
}

/** Whether a pending row counts as failed: an explicit "failed" status, or
 * the pre-status legacy shape (a lastError with no status field). Failed
 * rows are never auto-resent by the reconnect flush -- the user retries
 * them explicitly. Pure so the partition is testable without IndexedDB. */
export function isFailedPending(row: PendingLine): boolean {
  return (
    row.status === "failed" ||
    (row.status === undefined && row.lastError !== undefined)
  );
}

/** Flip a pending row to failed after a send attempt rejects (best-effort).
 * The reconnect flush partitions on this: failed rows are surfaced to the
 * user for an explicit retry, never auto-resent. The get and the put run
 * in ONE readwrite transaction so the write cannot resurrect a row the
 * ack path deleted between the read and the write. */
export async function markPendingFailed(
  tagmaId: string,
  localSeq: number,
  error: string,
): Promise<void> {
  try {
    const d = await durableDb();
    await new Promise<void>((resolve, reject) => {
      const tx = d.transaction(PENDING_STORE, "readwrite");
      const store = tx.objectStore(PENDING_STORE);
      const key = [tagmaId, localSeq];
      const getReq = store.get(key);
      getReq.onsuccess = () => {
        const row = getReq.result as PendingLine | undefined;
        if (row) store.put({ ...row, status: "failed", lastError: error });
      };
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("idb write failed"));
      tx.onabort = () => reject(tx.error ?? new Error("idb aborted"));
    });
  } catch {
    // best-effort
  }
}

/** Remove one pending row (the send landed, or the user discarded it). */
export async function deletePending(
  tagmaId: string,
  localSeq: number,
): Promise<void> {
  try {
    pendingDeleteObserver?.(tagmaId, localSeq);
    await runDurable("readwrite", (s) => s.delete([tagmaId, localSeq]));
  } catch {
    // best-effort
  }
}

/** Order pending rows oldest-first for flushing and rehydration.
 *  localSeq is -Date.now(): the MOST negative value is the NEWEST row,
 *  so descending numeric order (b - a) puts the oldest row first. */
export function sortPendingOldestFirst(rows: PendingLine[]): PendingLine[] {
  return rows.sort((a, b) => b.localSeq - a.localSeq);
}

/** Test seam: when non-null, readPendingByTagma returns this instead of
 * touching IndexedDB, so tests can inject a fixed row set and observe the
 * flush's ordering and partitioning. Production code never sets it. */
let pendingRowsOverride: ((tagmaId: string) => PendingLine[]) | null = null;

/** Install (or clear with null) the pending-rows test seam. */
export function setPendingRowsForTests(
  provide: ((tagmaId: string) => PendingLine[]) | null,
): void {
  pendingRowsOverride = provide;
}

/** Test seam: when set, every delete is reported to this observer on its way to
 *  IndexedDB, which `deno test` cannot reach. The delete itself stays
 *  best-effort by design (a missing store must not break a discard), so the
 *  observer is the only way a test can see that the row was dropped. Production
 *  never sets it. */
let pendingDeleteObserver:
  | ((tagmaId: string, localSeq: number) => void)
  | null = null;

/** Install (or clear with null) the pending-delete test seam. */
export function setPendingDeletesForTests(
  observe: ((tagmaId: string, localSeq: number) => void) | null,
): void {
  pendingDeleteObserver = observe;
}

/** Every unsent line for a tagma, oldest-first. Empty on any failure. */
export async function readPendingByTagma(
  tagmaId: string,
): Promise<PendingLine[]> {
  if (pendingRowsOverride) return pendingRowsOverride(tagmaId);
  try {
    const rows = await runDurable<PendingLine[]>("readonly", (s) =>
      s.getAll(
        IDBKeyRange.bound([tagmaId], [tagmaId, Number.MAX_SAFE_INTEGER]),
      ),
    );
    return sortPendingOldestFirst(rows);
  } catch {
    return [];
  }
}

/** Drop every pending row (logout: same shared-device privacy contract as
 * the message cache -- these carry the user's own unsent words). */
export async function clearPendings(): Promise<void> {
  try {
    await runDurable("readwrite", (s) => s.clear());
  } catch {
    // best-effort
  }
}

/** Test seam: shrink (or restore with null) the open budget. Production never
 *  sets it; a test needs a failure inside its own lifetime, not a full second. */
export function setOpenBudgetForTests(ms: number | null): void {
  openBudgetMs = ms ?? OPEN_BUDGET_MS;
}

/** Test seam: shrink (or restore with null) the cooldown, and release any
 *  cooldown already running, so a case does not have to wait one out. */
export function setCacheCooldownForTests(ms: number | null): void {
  cooldownMs = ms ?? COOLDOWN_MS;
  cooldownUntil = 0;
}
