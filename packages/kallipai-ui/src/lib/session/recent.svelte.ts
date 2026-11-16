// The reactive face of the recency ledger. Ordering is read during render
// (the sidebar and /chats both sort through recentConversations), so the
// entries live in a SvelteMap: a note from the envelope sink has to move a row
// in a list that is already on screen.
//
// One bucket per signed-in user, all of them in localStorage under
// `kallipai.recent.v1.<user id>`. Epoch milliseconds are derived facts, not
// user content, so localStorage (not sessionStorage) is the right home: the
// ordering should survive a restart the way the room list does.

import { SvelteMap } from "svelte/reactivity";
import {
  LEDGER_CAP,
  type Ledger,
  type RecentStorage,
  ledgerStorageKey,
  memoryRecentStorage,
  noteActivity,
  parseLedger,
  pruneLedger,
} from "./recent.ts";

/** localStorage behind the storage shape, degraded to memory when access
 *  throws or is undefined (SSR pass, sandboxed frame, privacy mode). The
 *  `typeof` guard keeps this module importable under Deno; a storage whose
 *  methods throw later is retired by `#readBucket`/`#writeBucket`. */
function localStorageOrFail(): RecentStorage {
  try {
    const held = typeof localStorage === "undefined" ? undefined : localStorage;
    if (!held) return memoryRecentStorage();
    return {
      getItem: (key) => held.getItem(key),
      setItem: (key, value) => held.setItem(key, value),
      removeItem: (key) => held.removeItem(key),
    };
  } catch {
    return memoryRecentStorage();
  }
}

export class RecentStore {
  #entries = new SvelteMap<string, number>();
  /** The handle the constructor received, kept even after it starts throwing.
   *  A write that fails says nothing about `removeItem` (over-quota storage
   *  usually still removes), and a signed-out account's ordering left on a
   *  shared browser is worse than a write that did not land. */
  #durable: RecentStorage;
  /** Where a degraded session reads and writes from then on, so a broken
   *  storage cannot break the ledger. */
  #memory: RecentStorage = memoryRecentStorage();
  #degraded = false;
  #userId: string | undefined;

  constructor(storage: RecentStorage = localStorageOrFail()) {
    this.#durable = storage;
  }

  /** Bind to the signed-in user and adopt their bucket. The previous user's
   *  entries are dropped first: on a shared browser the ledger must not follow
   *  the account that signed out. `undefined` (signed out) leaves it empty. */
  useUser(userId: string | undefined): void {
    if (userId === this.#userId) return;
    this.#userId = userId;
    this.#entries.clear();
    if (userId === undefined) return;
    const held = parseLedger(this.#readBucket(userId));
    for (const [key, at] of Object.entries(held)) this.#entries.set(key, at);
  }

  /** Record activity for `key`. Called from both the "I opened this" tick and
   *  the "I just sent here" one; the ledger keeps the later of the two.
   *  Stamps never move backwards, so a replayed or reordered event cannot
   *  demote a row the reader just used. */
  note(key: string, at: number = Date.now()): void {
    const held = this.#entries.get(key);
    if (held !== undefined && at <= held) return;
    this.#entries.set(key, at);
    this.#persist();
  }

  /** The last activity stamp for `key`, or undefined when never seen. */
  at(key: string): number | undefined {
    return this.#entries.get(key);
  }

  /** The whole ledger, for the selector. */
  ledger(): Ledger {
    return Object.fromEntries(this.#entries);
  }

  /** Drop every entry and unbind the user (sign-out). Only this user's
   *  bucket is touched; unbinding is what keeps a note from a still mounted
   *  page from writing the bucket back after the sign-out. */
  reset(): void {
    this.#entries.clear();
    const userId = this.#userId;
    this.#userId = undefined;
    if (userId !== undefined) this.#dropBucket(userId);
  }

  /** Read one bucket. A storage that throws here is retired for the rest of
   *  the session: the ledger is best-effort, and `note()` runs on the inbound
   *  hot path, where an escaping quota or security error would be read as a
   *  failed message by callers sitting between "sent" and "shown". */
  #readBucket(userId: string): string | null {
    if (!this.#degraded) {
      try {
        return this.#durable.getItem(ledgerStorageKey(userId));
      } catch {
        this.#degraded = true;
      }
    }
    return this.#memory.getItem(ledgerStorageKey(userId));
  }

  /** Write one bucket, retiring a storage that throws (see `#readBucket`). */
  #writeBucket(userId: string, value: string): void {
    if (!this.#degraded) {
      try {
        this.#durable.setItem(ledgerStorageKey(userId), value);
        return;
      } catch {
        this.#degraded = true;
      }
    }
    this.#memory.setItem(ledgerStorageKey(userId), value);
  }

  /** Drop one bucket. The durable handle is asked even when degraded: a write
   *  that hit the quota does not mean the removal will, and the bucket must
   *  not outlive the account on a shared browser. */
  #dropBucket(userId: string): void {
    try {
      this.#durable.removeItem(ledgerStorageKey(userId));
    } catch {
      this.#degraded = true;
    }
    this.#memory.removeItem(ledgerStorageKey(userId));
  }

  #persist(): void {
    // The bucket is the one bound at this moment, not the one the note began
    // under: buckets never cross users, entries can. A callback from a page
    // that outlived a sign-out lands its row under the new account's key.
    const userId = this.#userId;
    // Notes taken while signed out stay in memory only: there is no bucket
    // to write them into, and writing one would leave the next user's
    // ordering in a key nobody reads.
    if (userId === undefined) return;
    const pruned = pruneLedger(this.ledger());
    if (this.#entries.size > LEDGER_CAP) {
      this.#entries.clear();
      for (const [key, at] of Object.entries(pruned))
        this.#entries.set(key, at);
    }
    // Merge with what the bucket already holds rather than overwriting it:
    // another tab keeps its own snapshot, and a whole-bucket write would drop
    // the rows it saw. Per key the later stamp wins, the same rule memory
    // uses, so neither tab loses an ordering it knew about.
    let merged = parseLedger(this.#readBucket(userId));
    for (const [key, at] of Object.entries(pruned)) {
      merged = noteActivity(merged, key, at);
    }
    this.#writeBucket(userId, JSON.stringify(pruneLedger(merged)));
  }
}

export const recentStore = new RecentStore();
