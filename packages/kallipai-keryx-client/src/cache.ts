/// <reference lib="dom" />

import type { FileAttachment } from "@kallipai/kallipai-common";
// The per-device IndexedDB cache of already-loaded chat lines: a durable
// mirror of what the app has rendered, so a refresh/reopen restores the
// conversation from local state and only asks the tagma for an incremental
// delta (`History{after: maxRendered}`) instead of re-pulling the whole window.
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
}

const DB_NAME = "kallipai-keryx";
// v2: the keyPath field was renamed `convId` -> `conversationId`. v3: the row
// gained an optional `sender`. The messages store is a disposable derived
// cache (re-pulled from the tagma), so on upgrade we drop+recreate it
// rather than migrate rows. The pending store holds unsent sends and is
// created once and never dropped. v6: the 1:1 read watermarks live in the
// tagma, so the browser's copy is deleted on upgrade -- an old install would
// otherwise keep rows nothing reads.
const DB_VERSION = 6;
const STORE = "messages";
const PENDING_STORE = "pending";
/** The 1:1 watermark store that v4 added and v6 drops: one row per
 *  conversation. */
const V5_WATERMARK_STORE = "read_watermarks";

let dbPromise: Promise<IDBDatabase> | null = null;

/** Lazily open (and upgrade-create) the `kallipai-keryx` DB, after a
 * one-shot migration of any legacy `kallipai-relay` database (below).
 * Cached for the document lifetime; rejects if IndexedDB is unavailable. */
function openDb(): Promise<IDBDatabase> {
  return new Promise<IDBDatabase>((resolve, reject) => {
    if (typeof indexedDB === "undefined") {
      reject(new Error("IndexedDB unavailable"));
      return;
    }
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = (event) => {
      const d = req.result;
      // Drop any pre-v3 store (older keyPath or row shape) and recreate. The
      // cache is disposable and re-pulled from the tagma on the next open.
      if (event.oldVersion < 3 && d.objectStoreNames.contains(STORE)) {
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
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("indexedDB open failed"));
  });
}

// --- one-shot legacy migration (pre-split database) ---------------------------
//
// This package inherited its IndexedDB schema from the pre-split
// `kallipai-relay` database. The two matched at the rename; this side
// has moved since: it is at v6, it creates `messages` and `pending`,
// and it does not create the `read_watermarks` store, while a legacy
// database keeps that store at whatever version it reached (rows
// nothing reads). On the first open after the rename the durable rows
// move over and the legacy database is deleted. The messages store is
// deliberately left behind -- a disposable derived cache re-pulled from
// the tagma, so paying the copy cost buys nothing.

/** Migrate the legacy `kallipai-relay` database into this one, whenever the
 * legacy database exists. Durable rows only: pending (unsent) lines are
 * copied verbatim -- a dropped pending row is a lost message -- and the
 * disposable messages cache is skipped and re-pulled. Only pending rows
 * have anything to copy: 1:1 read state is not device-local any more.
 *
 * There is deliberately no "done" flag: the probe is the truth, because a
 * localStorage mark can drift from disk (a blocked delete leaves the
 * legacy database alive with the flag already set). An absent legacy
 * database costs one idempotent open that aborts its own upgrade. A
 * cross-database copy cannot be transactional, so the machine is ordered
 * for idempotent re-entry: every write is an upsert (a re-run rewrites the
 * same rows) and the legacy database is deleted only after the full copy,
 * so any interruption leaves a state the next run converges from. A
 * blocked delete (another tab still holds a legacy connection) still
 * completes the machine -- the copy is durable, and a later document's
 * probe re-copies whatever that tab wrote into the orphan, then reaps
 * it. */
const LEGACY_DB_NAME = "kallipai-relay";

let migrationPromise: Promise<void> | null = null;

export async function migrateLegacyCache(): Promise<"migrated" | "absent"> {
  const legacy = await openLegacyIfExists();
  if (!legacy) {
    return "absent";
  }
  // openDb, not db: db() awaits this very migration.
  const target = await openDb();
  const pendings = await readLegacyRows(legacy, PENDING_STORE);
  for (const row of pendings) {
    await putRowOn(target, PENDING_STORE, row);
  }
  legacy.close();
  await deleteLegacyDb();
  return "migrated";
}

function db(): Promise<IDBDatabase> {
  if (dbPromise) return dbPromise;
  dbPromise = ensureMigrated().then(() => openDb());
  return dbPromise;
}

/** Run the legacy migration at most once per document lifetime, ahead of the
 * first cache open. A failed migration must not take the cache down with
 * it: the promise settles and the next page load retries the probe, while
 * this session serves re-pullable state from the new database (worst case
 * a pending line stays invisible until the migration lands). The warn
 * fires at most once per document: the cached promise runs the machine
 * once. */
function ensureMigrated(): Promise<void> {
  if (!migrationPromise) {
    migrationPromise = migrateLegacyCache().then(
      () => undefined,
      (error) => {
        // A dropped migration retries on the next page load, but leaves
        // no trace without this; first-failure-only, the same shape as
        // RelayChannel's decrypt warn.
        console.warn(
          "[KeryxCache] legacy cache migration failed; retries next load",
          error,
        );
        return undefined;
      },
    );
  }
  return migrationPromise;
}

/** Open the legacy database without ever creating it: an upgrade event here
 * means it does not exist, and aborting the upgrade fails the open, which
 * maps to null. Stores absent from an older legacy version (pending is v5)
 * read as empty. */
function openLegacyIfExists(): Promise<IDBDatabase | null> {
  return new Promise((resolve, reject) => {
    if (typeof indexedDB === "undefined") {
      resolve(null);
      return;
    }
    const req = indexedDB.open(LEGACY_DB_NAME);
    req.onupgradeneeded = () => req.transaction?.abort();
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => {
      if (req.error?.name === "AbortError") resolve(null);
      else reject(req.error ?? new Error("legacy cache open failed"));
    };
  });
}

/** Upsert one row onto an open database handle. The migration cannot go
 * through run() -- run() opens via db(), which awaits this very
 * migration. Per-row transactions (not one per store): each write is an
 * idempotent upsert, so the granularity only sets the retry unit. */
function putRowOn(
  target: IDBDatabase,
  store: string,
  row: unknown,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const req = target
      .transaction(store, "readwrite")
      .objectStore(store)
      .put(row);
    req.onsuccess = () => resolve();
    req.onerror = () =>
      reject(req.error ?? new Error("legacy cache copy failed"));
  });
}

function readLegacyRows(
  source: IDBDatabase,
  store: string,
): Promise<unknown[]> {
  if (!source.objectStoreNames.contains(store)) return Promise.resolve([]);
  return new Promise((resolve, reject) => {
    const req = source
      .transaction(store, "readonly")
      .objectStore(store)
      .getAll();
    req.onsuccess = () => resolve(req.result as unknown[]);
    req.onerror = () =>
      reject(req.error ?? new Error("legacy cache read failed"));
  });
}

function deleteLegacyDb(): Promise<void> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.deleteDatabase(LEGACY_DB_NAME);
    req.onsuccess = () => resolve();
    // Blocked only means another tab holds a legacy connection; the copy is
    // durable and the orphan is reaped when that tab closes.
    req.onblocked = () => resolve();
    req.onerror = () =>
      reject(req.error ?? new Error("legacy cache delete failed"));
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

/** Run a single request on one store (default: the messages cache). */
function run<T>(
  mode: IDBTransactionMode,
  fn: (store: IDBObjectStore) => IDBRequest<T>,
  store: string = STORE,
): Promise<T> {
  return db().then(
    (d) =>
      new Promise<T>((resolve, reject) => {
        const req = fn(d.transaction(store, mode).objectStore(store));
        req.onsuccess = () => resolve(req.result);
        req.onerror = () =>
          reject(req.error ?? new Error("idb request failed"));
      }),
  );
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
    await run("readwrite", (s) => s.put(row), PENDING_STORE);
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
    const d = await db();
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
    await run("readwrite", (s) => s.delete([tagmaId, localSeq]), PENDING_STORE);
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
/** Every unsent line for a tagma, oldest-first. Empty on any failure. */
export async function readPendingByTagma(
  tagmaId: string,
): Promise<PendingLine[]> {
  if (pendingRowsOverride) return pendingRowsOverride(tagmaId);
  try {
    const rows = await run<PendingLine[]>(
      "readonly",
      (s) =>
        s.getAll(
          IDBKeyRange.bound([tagmaId], [tagmaId, Number.MAX_SAFE_INTEGER]),
        ),
      PENDING_STORE,
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
    await run("readwrite", (s) => s.clear(), PENDING_STORE);
  } catch {
    // best-effort
  }
}
