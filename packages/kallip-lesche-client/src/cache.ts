/// <reference lib="dom" />

import type { FileAttachment } from "@kallipai/kallip-common";
// The per-device IndexedDB cache of already-loaded chat lines: a durable
// mirror of what the app has rendered, so a refresh/reopen restores the
// conversation from local state and only asks the tagma for an incremental
// delta (`History{after: maxRendered}`) instead of re-pulling the whole window.
//
// This is a DUMB key-value store of `{conversationId, historyId, role, text}`
// tuples. It deliberately does NOT interpret `role` (a UI concept owned by
// kallip-ui's transcript reducer) — the UI writes tuples it extracted via its
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
}

const DB_NAME = "kallip-relay";
// v2: the keyPath field was renamed `convId` -> `conversationId`. v3: the row
// gained an optional `sender`. v4: added the `read_watermarks` store (the 1:1
// unread high-water). The messages store is a disposable derived cache
// (re-pulled from the tagma), so on upgrade we drop+recreate it rather than
// migrate rows; the watermark store is created once and never dropped.
const DB_VERSION = 5;
const STORE = "messages";
const WATERMARK_STORE = "read_watermarks";
const PENDING_STORE = "pending";

let dbPromise: Promise<IDBDatabase> | null = null;

/** Lazily open (and upgrade-create) the `kallip-relay` DB. Cached for the
 * document lifetime. Rejects if IndexedDB is unavailable. */
function db(): Promise<IDBDatabase> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise<IDBDatabase>((resolve, reject) => {
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
      if (!d.objectStoreNames.contains(WATERMARK_STORE)) {
        // v4: the per-tagma unread high-water, one row per tagma. Never
        // dropped on upgrade: recreating it empty would re-count every
        // delivered line as unread once the catch-up pull lands.
        d.createObjectStore(WATERMARK_STORE, { keyPath: "tagmaId" });
      }
      if (!d.objectStoreNames.contains(PENDING_STORE)) {
        // v5: unsent local messages, one row per attempt, keyed
        // [tagmaId, localSeq]. Survives upgrades like the watermark store:
        // these rows exist nowhere else -- a dropped row is a lost message.
        d.createObjectStore(PENDING_STORE, {
          keyPath: ["tagmaId", "localSeq"],
        });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error ?? new Error("indexedDB open failed"));
  });
  return dbPromise;
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

/** -- 1:1 unread high-water ---------------------------------------------------
 *
 *  The per-tagma watermark behind the unread badge (kallip-ui's unread
 *  store): the largest `chat_history.id` this device has counted or seen
 *  while viewing the tagma's channel. Persisted so a reload/restore keeps
 *  the count stable, and cleared on logout (shared-device privacy, same
 *  contract as the message cache). Same degrade-quietly contract: a failed
 *  read returns null (the store re-seeds from its first observed line); a
 *  failed write only means the next observe re-persists. */

interface ReadWatermarkRow {
  tagmaId: string;
  knownSeq: number;
}

/** The stored high-water for a tagma, or null when none / on any IndexedDB
 *  failure (private mode etc. -- the unread store then re-seeds). */
export async function getReadWatermark(
  tagmaId: string,
): Promise<number | null> {
  try {
    const row = await run<ReadWatermarkRow | undefined>(
      "readonly",
      (s) => s.get(tagmaId),
      WATERMARK_STORE,
    );
    return typeof row?.knownSeq === "number" ? row.knownSeq : null;
  } catch {
    return null;
  }
}

/** Persist the high-water for a tagma (upsert). Best-effort. */
export async function putReadWatermark(
  tagmaId: string,
  knownSeq: number,
): Promise<void> {
  try {
    await run(
      "readwrite",
      (s) => s.put({ tagmaId, knownSeq }),
      WATERMARK_STORE,
    );
  } catch {
    // best-effort; the next observe re-persists
  }
}

/** Drop every read watermark (logout). Best-effort. */
export async function clearReadWatermarks(): Promise<void> {
  try {
    await run("readwrite", (s) => s.clear(), WATERMARK_STORE);
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
  readonly createdAt: string;
  readonly lastError?: string;
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

/** Every unsent line for a tagma, oldest-first. Empty on any failure. */
export async function readPendingByTagma(
  tagmaId: string,
): Promise<PendingLine[]> {
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
