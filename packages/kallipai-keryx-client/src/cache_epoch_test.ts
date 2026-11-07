// The cache epoch: rows are stamped with the history numbering they were read
// under, and rows from another numbering are cleared in the same transaction
// that writes the new stamp. deno test ships no IndexedDB, so a small
// event-shaped fake models the surface the cache touches -- open (with
// version), upgrade handlers, transactions over several stores at once,
// put/get/delete/clear, and a cursor that reports exhaustion. Every case
// imports `cache.ts` under a unique query string, because the open and epoch
// promises are module state.

import type { CachedLine } from "./cache.ts";

type Row = Record<string, unknown>;
class FakeRequest<T> {
  result: T | undefined = undefined;
  error: DOMException | null = null;
  onsuccess: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onupgradeneeded: ((event: { oldVersion: number }) => void) | null = null;
  transaction: { abort: () => void } | null = null;
  deliver(result: T): void {
    this.result = result;
    queueMicrotask(() => this.onsuccess?.());
  }
  fail(name: string): void {
    this.error = { name } as DOMException;
    queueMicrotask(() => this.onerror?.());
  }
}

function respond<T>(result: T): FakeRequest<T> {
  const req = new FakeRequest<T>();
  req.deliver(result);
  return req;
}

class FakeStore {
  readonly entries = new Map<string, unknown>();
  constructor(readonly keyPath?: string | readonly string[]) {}

  private keyOf(value: unknown): string {
    const kp = this.keyPath;
    if (kp === undefined) return String(value);
    const row = value as Row;
    return Array.isArray(kp)
      ? kp.map((k) => String(row[k])).join("\u0000")
      : String(row[kp as string]);
  }

  /** A keyed store derives its key from the row; the key-value store (the
   *  `meta` one) takes it as an argument, exactly like the real API. */
  put(value: unknown, key?: unknown): FakeRequest<undefined> {
    this.entries.set(
      key === undefined ? this.keyOf(value) : String(key),
      value,
    );
    return respond(undefined);
  }

  get(key: unknown): FakeRequest<unknown> {
    return respond(this.entries.get(String(key)));
  }

  delete(key: unknown): FakeRequest<undefined> {
    this.entries.delete(String(key));
    return respond(undefined);
  }

  clear(): FakeRequest<undefined> {
    this.entries.clear();
    return respond(undefined);
  }

  getAll(): FakeRequest<unknown[]> {
    return respond([...this.entries.values()]);
  }

  openCursor(): FakeRequest<undefined> {
    return respond(undefined);
  }
}

/** A store whose requests never settle: what a body sees once its transaction
 *  has already failed to start. Nothing it writes is observed, which is how a
 *  refused transaction keeps its half of the bargain. */
class SilentStore extends FakeStore {
  override put(): FakeRequest<undefined> {
    return new FakeRequest<undefined>();
  }
  override get(): FakeRequest<unknown> {
    return new FakeRequest<unknown>();
  }
  override clear(): FakeRequest<undefined> {
    return new FakeRequest<undefined>();
  }
}

class FakeTransaction {
  error: DOMException | null = null;
  oncomplete: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  /** Set when the engine refused this transaction: its body runs against
   *  SilentStore and the abort fires on the next task. */
  dead = false;
  constructor(
    private readonly database: FakeDatabase,
    readonly stores: readonly string[],
  ) {}
  objectStore(name: string): FakeStore {
    return this.dead ? new SilentStore() : this.database.stores.get(name)!;
  }
}

class FakeDatabase {
  readonly stores = new Map<string, FakeStore>();
  onversionchange: (() => void) | null = null;
  closed = false;

  constructor(
    readonly name: string,
    public version: number,
  ) {}

  get objectStoreNames(): { contains: (name: string) => boolean } {
    return { contains: (name: string) => this.stores.has(name) };
  }

  createObjectStore(
    name: string,
    options?: { keyPath?: string | readonly string[] },
  ): FakeStore {
    const store = new FakeStore(options?.keyPath);
    this.stores.set(name, store);
    return store;
  }

  deleteObjectStore(name: string): void {
    this.stores.delete(name);
  }

  transaction(
    stores: string | readonly string[],
    _mode?: string,
  ): FakeTransaction {
    const names = typeof stores === "string" ? [stores] : stores;
    const tx = new FakeTransaction(this, names);
    // Completion lands on a task, after every request the body queued as a
    // microtask: an engine fires `complete` once the queue has drained.
    if (failNextEpoch && names.includes("meta")) {
      failNextEpoch = false;
      tx.error = Object.assign(new Error("quota"), {
        name: "QuotaExceededError",
      }) as DOMException;
      tx.dead = true;
      setTimeout(() => tx.onabort?.(), 0);
    } else {
      setTimeout(() => tx.oncomplete?.(), 0);
    }
    return tx;
  }

  close(): void {
    this.closed = true;
  }
}

const CURRENT = "kallipai-keryx";
const databases = new Map<string, FakeDatabase>();
/** The next transaction that touches `meta` aborts: a clear the engine refused
 *  (quota, a page being torn down) must not look like success. */
let failNextEpoch = false;

function upgrade(
  req: FakeRequest<FakeDatabase>,
  db: FakeDatabase,
  oldVersion: number,
): void {
  req.result = db;
  req.onupgradeneeded?.({ oldVersion });
}

const fakeIndexedDB = {
  open: (name: string, version?: number) => {
    const req = new FakeRequest<FakeDatabase>();
    queueMicrotask(() => {
      const existing = databases.get(name);
      const db = existing ?? new FakeDatabase(name, version ?? 1);
      if (!existing) databases.set(name, db);
      const oldVersion = existing ? existing.version : 0;
      if (oldVersion === 0 || (version !== undefined && version > oldVersion)) {
        upgrade(req, db, oldVersion);
        db.version = version ?? db.version;
      }
      req.deliver(db);
    });
    return req;
  },
};

function installFakes(): () => void {
  databases.clear();
  failNextEpoch = false;
  const g = globalThis as Record<string, unknown>;
  const prevIdb = g.indexedDB;
  const prevRange = g.IDBKeyRange;
  Object.defineProperty(globalThis, "indexedDB", {
    value: fakeIndexedDB,
    configurable: true,
  });
  Object.defineProperty(globalThis, "IDBKeyRange", {
    value: { bound: () => ({}) },
    configurable: true,
  });
  return () => {
    const prev = { indexedDB: prevIdb, IDBKeyRange: prevRange };
    for (const key of Object.keys(prev) as Array<keyof typeof prev>) {
      if (prev[key] === undefined) delete g[key];
      else {
        Object.defineProperty(globalThis, key, {
          value: prev[key],
          configurable: true,
        });
      }
    }
  };
}

function store(name: string): FakeStore {
  return databases.get(CURRENT)!.stores.get(name)!;
}

function seedRow(historyId: number): CachedLine {
  return {
    conversationId: "c-1",
    historyId,
    role: "user",
    text: "line " + historyId,
  };
}

const { assertEquals, assertRejects } = await import("@std/assert");

Deno.test(
  "rows read under another numbering are cleared, and the stamp rides along",
  async () => {
    const restore = installFakes();
    try {
      const cache = await import("./cache.ts?case=epoch-stale");
      cache.setCacheCooldownForTests(0);
      // No numbering: opens, stamps nothing (a clear is worse than a stale
      // row when the caller never said which numbering it reads).
      await cache.prepareCache();
      await cache.put(seedRow(1));
      store("pending").put({ tagmaId: "t-1", localSeq: 1, text: "unsent" });
      assertEquals(store("messages").entries.size, 1);
      assertEquals(store("meta").entries.size, 0);

      await cache.prepareCache(2);

      assertEquals(store("messages").entries.size, 0);
      assertEquals([...store("meta").entries.values()], [2]);
      // The unsent line exists nowhere else: a numbering change must not
      // reach it.
      assertEquals(store("pending").entries.size, 1);
    } finally {
      restore();
    }
  },
);

Deno.test("the same numbering leaves the rows in place", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=epoch-same");
    cache.setCacheCooldownForTests(0);
    await cache.prepareCache(2);
    await cache.put(seedRow(1));
    await cache.put(seedRow(2));
    assertEquals(store("messages").entries.size, 2);

    await cache.prepareCache(2);

    assertEquals(store("messages").entries.size, 2);
    assertEquals([...store("meta").entries.values()], [2]);
  } finally {
    restore();
  }
});

Deno.test(
  "a refused clear rejects and the next attempt runs the epoch again",
  async () => {
    const restore = installFakes();
    try {
      const cache = await import("./cache.ts?case=epoch-abort");
      cache.setCacheCooldownForTests(0);
      await cache.prepareCache();
      await cache.put(seedRow(1));
      failNextEpoch = true;

      await assertRejects(() => cache.prepareCache(2), cache.CacheUnavailable);

      // Neither half landed: the rows are still there, and no stamp claims
      // they belong to the new numbering.
      assertEquals(store("messages").entries.size, 1);
      assertEquals(store("meta").entries.size, 0);

      await cache.prepareCache(2);

      assertEquals(store("messages").entries.size, 0);
      assertEquals([...store("meta").entries.values()], [2]);
    } finally {
      restore();
    }
  },
);

Deno.test("concurrent prepares share one epoch transaction", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=epoch-concurrent");
    cache.setCacheCooldownForTests(0);
    await cache.prepareCache();
    await cache.put(seedRow(1));
    const meta = store("meta");
    const metaPut = meta.put.bind(meta);
    let stamps = 0;
    meta.put = (value: unknown, key?: unknown) => {
      stamps += 1;
      return metaPut(value, key);
    };

    await Promise.all([cache.prepareCache(2), cache.prepareCache(2)]);

    assertEquals(stamps, 1);
    assertEquals(store("messages").entries.size, 0);
  } finally {
    restore();
  }
});

Deno.test("a refused epoch transaction starts the cooldown", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=epoch-cooldown");
    cache.setCacheCooldownForTests(0);
    await cache.prepareCache();
    await cache.put(seedRow(1));
    const meta = store("meta");
    const metaPut = meta.put.bind(meta);
    let stamps = 0;
    meta.put = (value: unknown, key?: unknown) => {
      stamps += 1;
      return metaPut(value, key);
    };
    cache.setCacheCooldownForTests(60_000);
    failNextEpoch = true;

    await assertRejects(() => cache.prepareCache(2), cache.CacheUnavailable);

    // The failed check cools the cache down, so the retry a per-frame
    // catch-up would make runs no transaction at all.
    await assertRejects(() => cache.prepareCache(2), cache.CacheUnavailable);
    assertEquals(stamps, 0);
    assertEquals(store("messages").entries.size, 1);
    assertEquals(store("meta").entries.size, 0);
  } finally {
    restore();
  }
});
