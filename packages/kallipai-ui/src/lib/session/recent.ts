// Session recency: when each conversation last had activity on this device,
// and the ordering the sidebar and /chats derive from it. Nothing on the wire
// carries a time: a room view has `created_at` and the reader's cursor, and
// the daemon's session entry has neither. So the only activity facts are the
// ones this client observes: an envelope arriving, a page opening, and a
// message leaving the composer.
//
// The ledger stores `key -> epoch ms` and nothing else. Keys are chosen by the
// caller (roomKey / tagmaKey / pairKey), which keeps this module free of store
// imports and runnable from plain Deno tests without the Svelte compiler.

/** Sidebar rows. The full list on /chats is deliberately not capped: the
 *  manager lists everything, and only the sidebar has to fit a column. */
export const RECENT_CAP = 8;

/** Ledger entries kept per user. Older ones are dropped on write, so a long
 *  history of one-off rooms cannot grow the stored blob without bound. */
export const LEDGER_CAP = 200;

/** One conversation as its owner already lists it, plus the ledger key that
 *  owner computes for it. The selector never learns which kind a row is. */
export interface RecentRow {
  readonly key: string;
  /** Unread count when the surface has one. Rows with no ledger time use it
   *  to decide who goes first; rows with a time ignore it (recency wins). */
  readonly unread?: number;
}

export type Ledger = Readonly<Record<string, number>>;

/** Storage prefix. The user id is appended, so a shared browser does not hand
 *  one account's ordering to the next. */
export const LEDGER_PREFIX = "kallipai.recent.v1";

export function ledgerStorageKey(userId: string): string {
  return `${LEDGER_PREFIX}.${userId}`;
}

/** The storage surface the store mirrors into: one raw string per user.
 *  Narrowed so tests pass a Map-backed stub and a blocked storage degrades. */
export interface RecentStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** A no-op storage: the ledger then lives only for this page load. */
export function memoryRecentStorage(): RecentStorage {
  const data = new Map<string, string>();
  return {
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => void data.set(key, value),
    removeItem: (key) => void data.delete(key),
  };
}

/** Parse a stored ledger. Anything unreadable degrades to empty: a lost
 *  ordering costs one reshuffle, while a throw here would cost the sidebar. */
export function parseLedger(raw: string | null): Record<string, number> {
  if (raw === null) return {};
  try {
    const held: unknown = JSON.parse(raw);
    if (held === null || typeof held !== "object" || Array.isArray(held)) {
      return {};
    }
    const out: Record<string, number> = {};
    for (const [key, value] of Object.entries(
      held as Record<string, unknown>,
    )) {
      if (typeof value === "number" && Number.isFinite(value)) out[key] = value;
    }
    return out;
  } catch {
    return {};
  }
}

/** Record activity at `at`, never moving an entry backwards. Two writers can
 *  report out of order -- the open page ticks its own `seen` stamp while the
 *  envelope sink stamps the same room's arrival -- and the later fact is the
 *  one that keeps the row in place. */
export function noteActivity(
  ledger: Ledger,
  key: string,
  at: number,
): Record<string, number> {
  const held = ledger[key];
  return { ...ledger, [key]: held === undefined || at > held ? at : held };
}

/** Keep the LEDGER_CAP newest entries. Ties fall back to the literal key
 *  order, which is stable across calls and therefore across a reload (a
 *  locale-aware comparison would order the same keys differently in a
 *  different browser language). */
export function pruneLedger(ledger: Ledger): Record<string, number> {
  const entries = Object.entries(ledger);
  if (entries.length <= LEDGER_CAP) return { ...ledger };
  entries.sort(
    ([aKey, aAt], [bKey, bAt]) => bAt - aAt || (aKey < bKey ? -1 : 1),
  );
  const out: Record<string, number> = {};
  let taken = 0;
  for (const [key, at] of entries) {
    if (taken === LEDGER_CAP) break;
    out[key] = at;
    taken++;
  }
  return out;
}

/** The conversations to show, newest activity first.
 *
 *  Rows the ledger has never seen follow in registration order, with the
 *  unread ones ahead: the server already said something arrived there, and a
 *  reader opening the sidebar is looking for exactly that. A row that has both
 *  a time and unread replies still sorts by time -- "what did I touch last"
 *  and "what lit up" answer different questions, and the ledger answers the
 *  first one for every row it knows.
 *
 *  Ledger entries whose conversation is gone (left room, unpaired tagma) are
 *  not listed and not deleted: the store has no view of registrations, and a
 *  rejoin should find its old place rather than the tail. */
export function recentConversations<Row extends RecentRow>(
  rows: readonly Row[],
  ledger: Ledger,
): Row[] {
  const timed: Row[] = [];
  const untimed: Row[] = [];
  for (const row of rows) {
    if (ledger[row.key] === undefined) untimed.push(row);
    else timed.push(row);
  }
  timed.sort((a, b) => (ledger[b.key] ?? 0) - (ledger[a.key] ?? 0));
  const unreadFirst: Row[] = [];
  const rest: Row[] = [];
  for (const row of untimed) {
    if ((row.unread ?? 0) > 0) unreadFirst.push(row);
    else rest.push(row);
  }
  return [...timed, ...unreadFirst, ...rest];
}

/** The sidebar's cut of {@link recentConversations}. The only place that
 *  slices: /chats shows the same order whole, and a second slice elsewhere
 *  would let the two lists disagree about where the overview ends. */
export function capRecent<Row extends RecentRow>(rows: readonly Row[]): Row[] {
  return rows.slice(0, RECENT_CAP);
}
