// The open contract of the per-device cache: one request in flight, one budget
// over the whole chain, a permit before any open, and a cooldown between
// attempts. deno test ships no IndexedDB, so a small event-shaped fake models
// exactly the surface the cache touches -- open (with version), upgrade
// transactions, put/get/getAll/delete/clear, a cursor that reports exhaustion,
// close, and the blocked event an upgrade fires when another tab holds the
// older version open. Every case imports `cache.ts` under a unique query
// string, because the open promise is module state.

type Row = Record<string, unknown>;

class FakeRequest<T> {
  result: T | undefined = undefined;
  error: DOMException | null = null;
  onsuccess: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onblocked: (() => void) | null = null;
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

class FakeStore {
  readonly rows = new Map<string, Row>();
  constructor(readonly keyPath: string | readonly string[] | undefined) {}

  private keyOf(key: unknown): string {
    if (Array.isArray(key)) return key.map(String).join("\u0000");
    const kp = this.keyPath;
    if (kp === undefined) throw new Error("store has no keyPath");
    const parts = Array.isArray(kp)
      ? kp.map((k) => String((key as Row)[k]))
      : [String((key as Row)[kp as string])];
    return parts.join("\u0000");
  }

  put(row: Row): FakeRequest<unknown> {
    this.rows.set(this.keyOf(row), row);
    return this.respond(undefined);
  }

  delete(key: unknown): FakeRequest<unknown> {
    this.rows.delete(this.keyOf(key));
    return this.respond(undefined);
  }

  clear(): FakeRequest<unknown> {
    this.rows.clear();
    return this.respond(undefined);
  }

  getAll(): FakeRequest<Row[]> {
    return this.respond([...this.rows.values()]);
  }

  get(key: unknown): FakeRequest<Row | undefined> {
    return this.respond(this.rows.get(this.keyOf(key)));
  }

  /** A cursor over an exhausted store: one success with no cursor at all, which
   *  is how the real API reports the end of a range. */
  openCursor(): FakeRequest<undefined> {
    return this.respond(undefined);
  }

  private respond<T>(result: T): FakeRequest<T> {
    const req = new FakeRequest<T>();
    req.deliver(result);
    return req;
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

  transaction(store: string): {
    objectStore: (name?: string) => FakeStore;
    oncomplete: (() => void) | null;
    onerror: (() => void) | null;
    onabort: (() => void) | null;
  } {
    const tx = {
      objectStore: (name?: string) => this.stores.get(name ?? store)!,
      oncomplete: null as (() => void) | null,
      onerror: null as (() => void) | null,
      onabort: null as (() => void) | null,
    };
    queueMicrotask(() => tx.oncomplete?.());
    return tx;
  }

  close(): void {
    this.closed = true;
  }
}

const CURRENT = "kallipai-keryx";

const databases = new Map<string, FakeDatabase>();
/** Every open, in order: the fake's only witness of how many times a document
 *  asked for a connection. */
let opens: string[] = [];
/** Open requests held back by `hangDatabaseOpen` / `blockDatabaseOpen`, so a
 *  case can complete them after the fact (a late connection must be closed). */
let held: FakeRequest<FakeDatabase>[] = [];
let hangDatabaseOpen = false;
let blockDatabaseOpen = false;

/** Run the upgrade handlers the cache installed, and report whether one of
 *  them aborted the upgrade (the "database does not exist" probe). */
function upgrade(
  req: FakeRequest<FakeDatabase>,
  db: FakeDatabase,
  oldVersion: number,
): boolean {
  req.result = db; // a real engine exposes the db during upgrade
  let abort = false;
  req.transaction = {
    abort: () => {
      abort = true;
    },
  };
  req.onupgradeneeded?.({ oldVersion });
  return abort;
}

const fakeIndexedDB = {
  open: (name: string, version?: number) => {
    opens.push(name);
    const req = new FakeRequest<FakeDatabase>();
    if (name === CURRENT && hangDatabaseOpen) {
      held.push(req);
      return req;
    }
    if (name === CURRENT && blockDatabaseOpen) {
      held.push(req);
      queueMicrotask(() => req.onblocked?.());
      return req;
    }
    queueMicrotask(() => {
      const existing = databases.get(name);
      const db = existing ?? new FakeDatabase(name, version ?? 1);
      if (!existing) databases.set(name, db);
      const oldVersion = existing ? existing.version : 0;
      if (oldVersion === 0 || (version !== undefined && version > oldVersion)) {
        const aborted = upgrade(req, db, oldVersion);
        if (aborted) {
          if (!existing) databases.delete(name);
          req.fail("AbortError");
          return;
        }
        db.version = version ?? db.version;
      }
      req.deliver(db);
    });
    return req;
  },
};

function installFakes(): () => void {
  databases.clear();
  opens = [];
  held = [];
  hangDatabaseOpen = false;
  blockDatabaseOpen = false;
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

function currentOpens(): number {
  return opens.filter((name) => name === CURRENT).length;
}

const { assertEquals, assertRejects } = await import("@std/assert");

Deno.test(
  "a blocked open rejects at once and no write retries it",
  async () => {
    const restore = installFakes();
    try {
      blockDatabaseOpen = true;
      const cache = await import("./cache.ts?case=open-blocked");
      cache.setCacheCooldownForTests(60_000);
      await assertRejects(() => cache.prepareCache(), cache.CacheUnavailable);
      assertEquals(currentOpens(), 1);
      // A read degrades to "no cache" instead of waiting or throwing.
      assertEquals(await cache.readTail("c-1", 10), []);
      await cache.put({
        conversationId: "c-1",
        historyId: 1,
        role: "user",
        text: "line",
      });
      // The per-frame write did NOT ask for another open: that is the storm
      // the permit rule exists to prevent.
      assertEquals(currentOpens(), 1);
      // An attach inside the cooldown refuses without opening.
      await assertRejects(() => cache.prepareCache(), cache.CacheUnavailable);
      assertEquals(currentOpens(), 1);
      // The user's retry is the one caller a cooldown does not turn away.
      blockDatabaseOpen = false;
      cache.permitRetry();
      await cache.prepareCache();
      assertEquals(currentOpens(), 2);
      await cache.put({
        conversationId: "c-1",
        historyId: 1,
        role: "user",
        text: "line",
      });
      assertEquals(
        databases.get(CURRENT)!.stores.get("messages")!.rows.size,
        1,
      );
    } finally {
      restore();
    }
  },
);

Deno.test("an open that never settles is cut off by the budget", async () => {
  const restore = installFakes();
  try {
    hangDatabaseOpen = true;
    const cache = await import("./cache.ts?case=open-budget");
    cache.setOpenBudgetForTests(20);
    const started = Date.now();
    await assertRejects(() => cache.prepareCache(), cache.CacheUnavailable);
    const elapsed = Date.now() - started;
    if (elapsed >= 900) {
      throw new Error(`the budget did not cut the open short: ${elapsed}ms`);
    }
    assertEquals(currentOpens(), 1);
    // The held request completes afterwards: that connection must be closed,
    // or it holds a version block over the next page load.
    const late = new FakeDatabase(CURRENT, 7);
    databases.set(CURRENT, late);
    held.shift()!.deliver(late);
    await new Promise((resolve) => setTimeout(resolve, 0));
    assertEquals(late.closed, true);
  } finally {
    restore();
  }
});

Deno.test("a connection arriving after a blocked open is closed", async () => {
  const restore = installFakes();
  try {
    blockDatabaseOpen = true;
    const cache = await import("./cache.ts?case=open-blocked-late");
    await assertRejects(() => cache.prepareCache(), cache.CacheUnavailable);
    const late = new FakeDatabase(CURRENT, 7);
    databases.set(CURRENT, late);
    held.shift()!.deliver(late);
    await new Promise((resolve) => setTimeout(resolve, 0));
    assertEquals(late.closed, true);
  } finally {
    restore();
  }
});

Deno.test(
  "a version change releases the connection, and only a permit reopens",
  async () => {
    const restore = installFakes();
    try {
      const cache = await import("./cache.ts?case=open-versionchange");
      await cache.prepareCache();
      const db = databases.get(CURRENT)!;
      assertEquals(db.closed, false);
      db.onversionchange!();
      assertEquals(db.closed, true);
      const opened = currentOpens();
      // No permit: a write degrades silently and asks for nothing.
      await cache.put({
        conversationId: "c-1",
        historyId: 1,
        role: "user",
        text: "line",
      });
      assertEquals(currentOpens(), opened);
      // The explicit retry reopens.
      await cache.prepareCache();
      assertEquals(currentOpens(), opened + 1);
    } finally {
      restore();
    }
  },
);

Deno.test("an open in flight is shared, never doubled", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=open-single-flight");
    cache.setOpenBudgetForTests(500);
    hangDatabaseOpen = true;
    const first = cache.prepareCache();
    // Let the first attempt reach the fake before the second caller asks.
    await new Promise((resolve) => setTimeout(resolve, 0));
    // The durable write takes its own permit; db() then joins the request
    // already in flight instead of opening a second connection.
    const second = cache.putPending({
      tagmaId: "t-2",
      localSeq: -1,
      text: "unsent",
      createdAt: "2026-11-07T00:00:00Z",
    });
    const db = new FakeDatabase(CURRENT, 7);
    db.createObjectStore("messages", {
      keyPath: ["conversationId", "historyId"],
    });
    db.createObjectStore("pending", { keyPath: ["tagmaId", "localSeq"] });
    databases.set(CURRENT, db);
    held.shift()!.deliver(db);
    await Promise.all([first, second]);
    assertEquals(currentOpens(), 1);
    assertEquals(db.stores.get("pending")!.rows.size, 1);
  } finally {
    restore();
  }
});

Deno.test("a durable pending write opens on its own", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=open-durable");
    await cache.putPending({
      tagmaId: "t-2",
      localSeq: -1,
      text: "unsent",
      createdAt: "2026-11-07T00:00:00Z",
    });
    assertEquals(currentOpens(), 1);
    const rows = await cache.readPendingByTagma("t-2");
    assertEquals(
      rows.map((row: { text: string }) => row.text),
      ["unsent"],
    );
  } finally {
    restore();
  }
});

Deno.test(
  "a durable write that joins the connection leaves no permit",
  async () => {
    const restore = installFakes();
    try {
      const cache = await import("./cache.ts?case=open-permit-residue");
      await cache.prepareCache();
      assertEquals(currentOpens(), 1);
      // The durable write takes a permit and then finds the live connection:
      // that permit must not survive as standing permission to reopen later.
      await cache.putPending({
        tagmaId: "t-3",
        localSeq: -1,
        text: "unsent",
        createdAt: "2026-11-07T00:00:00Z",
      });
      databases.get(CURRENT)!.onversionchange!();
      const opened = currentOpens();
      await cache.put({
        conversationId: "c-1",
        historyId: 1,
        role: "user",
        text: "line",
      });
      assertEquals(currentOpens(), opened);
      await cache.prepareCache();
      assertEquals(currentOpens(), opened + 1);
    } finally {
      restore();
    }
  },
);

Deno.test("a fresh database opens empty and keeps its stores", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=open-fresh");
    await cache.prepareCache();
    assertEquals(databases.has(CURRENT), true);
    assertEquals([...databases.get(CURRENT)!.stores.keys()].sort(), [
      "messages",
      "meta",
      "pending",
    ]);
    assertEquals(await cache.readTail("c-1", 10), []);
    assertEquals(await cache.readPendingByTagma("t-x"), []);
  } finally {
    restore();
  }
});
