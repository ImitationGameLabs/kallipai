// The legacy-cache migration machine, exercised end to end against a
// minimal event-shaped IndexedDB fake. deno test ships no IndexedDB (nor
// IDBKeyRange), so the fake models only the surface the cache and the
// migration touch -- open with optional version, upgrade transactions,
// getAll/get/put/clear, openCursor over an empty store, close,
// deleteDatabase (optionally blocked) -- firing events on microtasks like
// case installs fresh fakes and re-imports the cache module under a unique
// query string, because the open and migration promises are module state.

type FakeRow = Record<string, unknown>;

class FakeRequest<T> {
  result: T | undefined = undefined;
  error: DOMException | null = null;
  onsuccess: (() => void) | null = null;
  onblocked: (() => void) | null = null;
  onerror: (() => void) | null = null;
  fire(result: T) {
    this.result = result;
    queueMicrotask(() => this.onsuccess?.());
  }
  fail(name: string) {
    this.error = { name } as DOMException;
    queueMicrotask(() => this.onerror?.());
  }
}

/** An open request that can fire an upgrade event whose abort fails the
 * open with an AbortError -- the "database does not exist" probe. */
class FakeOpenRequest extends FakeRequest<FakeDatabase> {
  onupgradeneeded: ((event: { oldVersion: number }) => void) | null = null;
  transaction: { abort: () => void } | null = null;
  upgrade(db: FakeDatabase, oldVersion: number): boolean {
    this.result = db; // a real engine exposes the db during upgrade
    let abort = false;
    this.transaction = {
      abort: () => {
        abort = true;
      },
    };
    this.onupgradeneeded?.({ oldVersion });
    return abort;
  }
}

/** Composite-key rows are stringified ("a/b") for map storage, so a key
 * range's leading element selects by prefix split. Only the shapes the
 * cache dials (bound on the leading composite element) are modelled. */
class FakeKeyRange {
  constructor(readonly lower: unknown) {}
  selects(key: string): boolean {
    return Array.isArray(this.lower)
      ? key.split("/")[0] === String(this.lower[0])
      : key === String(this.lower);
  }
}

interface FakeStore {
  keyPath: string | string[];
  rows: Map<string, FakeRow>;
}

class FakeDatabase {
  readonly stores = new Map<string, FakeStore>();
  objectStoreNames = { contains: (name: string) => this.stores.has(name) };
  constructor(
    readonly name: string,
    public version: number,
  ) {}

  createObjectStore(name: string, opts: { keyPath: string | string[] }) {
    this.stores.set(name, { keyPath: opts.keyPath, rows: new Map() });
  }
  deleteObjectStore(name: string) {
    this.stores.delete(name);
  }

  private keyOf(store: FakeStore, row: FakeRow): string {
    const kp = store.keyPath;
    return Array.isArray(kp)
      ? kp.map((k) => String(row[k])).join("/")
      : String(row[kp]);
  }

  transaction(store: string, _mode: string) {
    const s = this.stores.get(store);
    if (!s) throw new Error(`no such store: ${store}`);
    return {
      objectStore: () => ({
        getAll: (range?: FakeKeyRange) => {
          const req = new FakeRequest<FakeRow[]>();
          const rows = [...s.rows.entries()]
            .filter(([k]) => (range ? range.selects(k) : true))
            .map(([, row]) => row);
          req.fire(rows);
          return req;
        },
        get: (key: string) => {
          const req = new FakeRequest<FakeRow | undefined>();
          req.fire(s.rows.get(key));
          return req;
        },
        put: (row: FakeRow) => {
          const req = new FakeRequest<string>();
          if (failNextPuts > 0) {
            failNextPuts -= 1;
            req.fail("UnknownError");
          } else {
            const key = this.keyOf(s, row);
            s.rows.set(key, row);
            req.fire(key);
          }
          return req;
        },
        clear: () => {
          const req = new FakeRequest<undefined>();
          s.rows.clear();
          req.fire(undefined);
          return req;
        },
        // Only the exhausted-cursor path is exercised (asserts over empty
        // message stores); ordering support is intentionally minimal.
        openCursor: (range?: FakeKeyRange) => {
          const req = new FakeRequest<{ value: FakeRow } | undefined>();
          const rows = [...s.rows.entries()].filter(([k]) =>
            range ? range.selects(k) : true,
          );
          queueMicrotask(() => {
            if (rows.length === 0) {
              req.result = undefined;
              req.onsuccess?.();
            }
          });
          return req;
        },
      }),
    };
  }
  close() {
    // connection accounting is not modelled; deletes never block here
  }
}

const databases = new Map<string, FakeDatabase>();
let failNextPuts = 0;
let blockNextDelete = false;

const fakeIndexedDB = {
  open: (name: string, version?: number) => {
    const req = new FakeOpenRequest();
    queueMicrotask(() => {
      const existing = databases.get(name);
      const db = existing ?? new FakeDatabase(name, version ?? 1);
      if (!existing) databases.set(name, db);
      const oldVersion = existing ? existing.version : 0;
      if (oldVersion === 0 || (version !== undefined && version > oldVersion)) {
        const aborted = req.upgrade(db, oldVersion);
        if (aborted) {
          if (!existing) databases.delete(name);
          req.fail("AbortError");
          return;
        }
      }
      req.fire(db);
    });
    return req;
  },
  deleteDatabase: (name: string) => {
    const req = new FakeRequest<undefined>();
    queueMicrotask(() => {
      if (blockNextDelete) {
        // a blocked delete: the db survives and neither success nor
        // error fires; the machine resolves via onblocked
        blockNextDelete = false;
        req.onblocked?.();
        return;
      }
      databases.delete(name);
      req.fire(undefined);
    });
    return req;
  },
};

function installFakes(): () => void {
  // each case starts from a clean world (module state, not per-case)
  databases.clear();
  failNextPuts = 0;
  blockNextDelete = false;
  const g = globalThis as Record<string, unknown>;
  const prevIdb = g.indexedDB;
  const prevRange = g.IDBKeyRange;
  Object.defineProperty(globalThis, "indexedDB", {
    value: fakeIndexedDB,
    configurable: true,
  });
  Object.defineProperty(globalThis, "IDBKeyRange", {
    value: { bound: (lower: unknown) => new FakeKeyRange(lower) },
    configurable: true,
  });
  return () => {
    const prev = {
      indexedDB: prevIdb,
      IDBKeyRange: prevRange,
    };
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

function seedLegacy(
  version: number,
  rows: {
    messages?: FakeRow[];
    pendings?: FakeRow[];
  },
) {
  const db = new FakeDatabase("kallipai-relay", version);
  if (version >= 3 || rows.messages) {
    db.createObjectStore("messages", {
      keyPath: ["conversationId", "historyId"],
    });
  }
  if (version >= 5 || rows.pendings) {
    db.createObjectStore("pending", { keyPath: ["tagmaId", "localSeq"] });
  }
  const put = (store: string, row: FakeRow) => {
    db.transaction(store, "readwrite").objectStore().put(row);
  };
  for (const row of rows.messages ?? []) put("messages", row);
  for (const row of rows.pendings ?? []) put("pending", row);
  databases.set("kallipai-relay", db);
}

/** Seed the CURRENT database at an older version, so the next open runs the
 *  upgrade over a populated world (the dropped store beside live data). */
function seedCurrent(
  version: number,
  rows: { watermarks?: FakeRow[]; pendings?: FakeRow[] },
) {
  const db = new FakeDatabase("kallipai-keryx", version);
  db.createObjectStore("messages", {
    keyPath: ["conversationId", "historyId"],
  });
  db.createObjectStore("read_watermarks", { keyPath: "tagmaId" });
  db.createObjectStore("pending", { keyPath: ["tagmaId", "localSeq"] });
  const put = (store: string, row: FakeRow) => {
    db.transaction(store, "readwrite").objectStore().put(row);
  };
  for (const row of rows.watermarks ?? []) put("read_watermarks", row);
  for (const row of rows.pendings ?? []) put("pending", row);
  databases.set("kallipai-keryx", db);
}

const { assertEquals } = await import("@std/assert");

Deno.test(
  "legacy migration copies durable rows and drops the legacy db",
  async () => {
    const restore = installFakes();
    try {
      seedLegacy(5, {
        pendings: [
          {
            tagmaId: "t-1",
            localSeq: -1000,
            text: "older unsent",
            createdAt: "2026-10-08T01:00:00Z",
          },
          {
            tagmaId: "t-1",
            localSeq: -2000,
            text: "newer unsent",
            createdAt: "2026-10-08T02:00:00Z",
          },
        ],
        messages: [
          { conversationId: "c-1", historyId: 7, role: "user", text: "line" },
        ],
      });
      // The ?case= suffix busts Deno's module cache: every import
      // re-evaluates the module, resetting its open/migration state.
      const cache = await import("./cache.ts?case=full");
      // The first open runs the migration; the pending read then observes
      // the migrated rows (the UI's unsent semantics survive the rename,
      // oldest first).
      const pendings = await cache.readPendingByTagma("t-1");
      assertEquals(
        pendings.map((p) => p.localSeq),
        [-1000, -2000],
      );
      // messages are disposable: not copied, re-pulled on demand
      assertEquals(await cache.readTail("c-1", 50), []);
      assertEquals(databases.has("kallipai-relay"), false);
      assertEquals(await cache.migrateLegacyCache(), "absent");
    } finally {
      restore();
    }
  },
);

Deno.test("no legacy database: cache opens empty", async () => {
  const restore = installFakes();
  try {
    const cache = await import("./cache.ts?case=absent");
    assertEquals(await cache.readPendingByTagma("t-x"), []);
    assertEquals(databases.has("kallipai-relay"), false);
    assertEquals(await cache.migrateLegacyCache(), "absent");
  } finally {
    restore();
  }
});

Deno.test(
  "a pre-v4 legacy database migrates as empty durable stores",
  async () => {
    const restore = installFakes();
    try {
      seedLegacy(3, {
        messages: [
          { conversationId: "c-1", historyId: 7, role: "user", text: "line" },
        ],
      });
      const cache = await import("./cache.ts?case=oldver");
      assertEquals(await cache.readPendingByTagma("t-1"), []);
      assertEquals(databases.has("kallipai-relay"), false);
    } finally {
      restore();
    }
  },
);

Deno.test("a partial copy failure converges on the next run", async () => {
  const restore = installFakes();
  let warns = 0;
  const origWarn = console.warn;
  console.warn = () => {
    warns += 1;
  };
  try {
    seedLegacy(5, {
      pendings: [
        {
          tagmaId: "t-1",
          localSeq: -1000,
          text: "older unsent",
          createdAt: "2026-10-08T01:00:00Z",
        },
        {
          tagmaId: "t-1",
          localSeq: -2000,
          text: "newer unsent",
          createdAt: "2026-10-08T02:00:00Z",
        },
      ],
    });
    // The first pending put fails: the machine aborts mid-copy, the
    // legacy db survives, and the cache still opens.
    failNextPuts = 1;
    const first = await import("./cache.ts?case=partial");
    assertEquals(await first.readPendingByTagma("t-1"), []);
    assertEquals(warns, 1);
    assertEquals(databases.has("kallipai-relay"), true);
    // A fresh module (next page load) re-runs the machine: upserts
    // converge and the legacy db is deleted.
    failNextPuts = 0;
    const second = await import("./cache.ts?case=retry");
    const pendings = await second.readPendingByTagma("t-1");
    assertEquals(
      pendings.map((p) => p.localSeq),
      [-1000, -2000],
    );
    assertEquals(databases.has("kallipai-relay"), false);
    assertEquals(warns, 1);
  } finally {
    failNextPuts = 0;
    console.warn = origWarn;
    restore();
  }
});

Deno.test(
  "a legacy db reappearing after migration is probed and re-copied",
  async () => {
    const restore = installFakes();
    try {
      // First document migrates cleanly and reaps the legacy db.
      seedLegacy(5, {
        pendings: [
          {
            tagmaId: "t-1",
            localSeq: -1000,
            text: "first unsent",
            createdAt: "2026-10-08T01:00:00Z",
          },
        ],
      });
      const first = await import("./cache.ts?case=reappear");
      assertEquals(
        (await first.readPendingByTagma("t-1")).map((p) => p.text),
        ["first unsent"],
      );
      assertEquals(databases.has("kallipai-relay"), false);
      // A still-open legacy tab (its delete was blocked) later writes
      // an unsent line into the orphaned db.
      seedLegacy(5, {
        pendings: [
          {
            tagmaId: "t-1",
            localSeq: -1000,
            text: "late unsent",
            createdAt: "2026-10-08T03:00:00Z",
          },
        ],
      });
      // A later document probes instead of trusting a done mark: the
      // late pending line is rescued and the orphan reaped again.
      const second = await import("./cache.ts?case=rescue");
      assertEquals(
        (await second.readPendingByTagma("t-1")).map((p) => p.text),
        ["late unsent"],
      );
      assertEquals(databases.has("kallipai-relay"), false);
    } finally {
      restore();
    }
  },
);

Deno.test(
  "a blocked delete still completes, and a later document reaps the orphan",
  async () => {
    const restore = installFakes();
    try {
      seedLegacy(5, {
        pendings: [
          {
            tagmaId: "t-1",
            localSeq: -1000,
            text: "unsent",
            createdAt: "2026-10-08T01:00:00Z",
          },
        ],
      });
      blockNextDelete = true;
      const first = await import("./cache.ts?case=blocked-del");
      // the copy is durable but the legacy db survived the blocked delete
      assertEquals(
        (await first.readPendingByTagma("t-1")).map((p) => p.text),
        ["unsent"],
      );
      assertEquals(databases.has("kallipai-relay"), true);
      // a later document probes, re-copies (a no-op upsert), and deletes
      const second = await import("./cache.ts?case=blocked-retry");
      assertEquals(
        (await second.readPendingByTagma("t-1")).map((p) => p.text),
        ["unsent"],
      );
      assertEquals(databases.has("kallipai-relay"), false);
    } finally {
      blockNextDelete = false;
      restore();
    }
  },
);

Deno.test(
  "the upgrade deletes the watermark store and leaves the pending rows",
  async () => {
    const restore = installFakes();
    try {
      seedCurrent(5, {
        watermarks: [{ tagmaId: "t-1", knownSeq: 9 }],
        pendings: [
          {
            tagmaId: "t-1",
            localSeq: -1000,
            text: "unsent line",
            createdAt: "2026-10-08T01:00:00Z",
          },
        ],
      });
      const cache = await import("./cache.ts?case=watermarks");
      // Opening the current database is what runs the upgrade.
      const pendings = await cache.readPendingByTagma("t-1");
      // The unsent line is still there: it exists nowhere else, so the upgrade
      // must not touch it.
      assertEquals(
        pendings.map((p) => p.localSeq),
        [-1000],
      );
      const db = databases.get("kallipai-keryx");
      assertEquals(db?.stores.has("read_watermarks"), false);
      assertEquals([...(db?.stores.keys() ?? [])].sort(), [
        "messages",
        "pending",
      ]);
    } finally {
      restore();
    }
  },
);
