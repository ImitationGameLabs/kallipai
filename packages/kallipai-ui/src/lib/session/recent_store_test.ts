// Tests for the reactive recency store: per-user buckets, write-through,
// never-backwards stamps, the ledger cap applied on write, and reset. Storage
// is stubbed with a Map so no real localStorage is touched (Deno has none).

import { assertEquals } from "@std/assert";
import { LEDGER_CAP, type RecentStorage, ledgerStorageKey } from "./recent.ts";
import { RecentStore } from "./recent.svelte.ts";

function memoryStorage(seed?: Record<string, string>): RecentStorage {
  const data = new Map(Object.entries(seed ?? {}));
  return {
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => void data.set(key, value),
    removeItem: (key) => void data.delete(key),
  };
}

Deno.test("a note lands in the user's bucket and survives a reload", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  assertEquals(store.at("room:a"), 500);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), '{"room:a":500}');

  const reloaded = new RecentStore(storage);
  reloaded.useUser("u-1");
  assertEquals(reloaded.at("room:a"), 500);
});

Deno.test("each user keeps their own ordering", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  store.useUser("u-2");
  assertEquals(store.at("room:a"), undefined);
  store.note("room:b", 600);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), '{"room:a":500}');
  assertEquals(storage.getItem(ledgerStorageKey("u-2")), '{"room:b":600}');
  store.useUser("u-1");
  assertEquals(store.at("room:b"), undefined);
  assertEquals(store.at("room:a"), 500);
});

Deno.test("an earlier stamp does not overwrite a later one", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 900);
  store.note("room:a", 100);
  assertEquals(store.at("room:a"), 900);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), '{"room:a":900}');
});

Deno.test("writing past the cap drops the oldest entries", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  for (let i = 0; i < LEDGER_CAP; i++) store.note(`room:${i}`, 1000 + i);
  store.note("room:newest", 5000);
  assertEquals(store.at("room:0"), undefined);
  assertEquals(store.at("room:newest"), 5000);
  assertEquals(store.ledger()["room:1"], 1001);
  const stored = JSON.parse(storage.getItem(ledgerStorageKey("u-1"))!);
  assertEquals(Object.keys(stored).length, LEDGER_CAP);
  assertEquals(stored["room:newest"], 5000);
});

Deno.test("reset clears memory and only this user's bucket", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  store.useUser("u-2");
  store.note("room:b", 600);
  store.reset();
  assertEquals(store.at("room:b"), undefined);
  assertEquals(storage.getItem(ledgerStorageKey("u-2")), null);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), '{"room:a":500}');
});

Deno.test("a storage that throws on write degrades to memory", () => {
  const data = new Map<string, string>();
  let writes = 0;
  const storage: RecentStorage = {
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => {
      writes++;
      if (writes > 1) throw new Error("QuotaExceededError");
      data.set(key, value);
    },
    removeItem: (key) => void data.delete(key),
  };
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  store.note("room:b", 600);
  assertEquals(store.at("room:b"), 600);
  assertEquals(store.ledger()["room:a"], 500);
});

Deno.test("a storage that throws on read degrades to memory", () => {
  const storage: RecentStorage = {
    getItem: () => {
      throw new Error("SecurityError");
    },
    setItem: () => {
      throw new Error("SecurityError");
    },
    removeItem: () => {
      throw new Error("SecurityError");
    },
  };
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  assertEquals(store.at("room:a"), 500);
});

Deno.test("reset unbinds so a late note cannot rewrite the bucket", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  store.reset();
  store.note("room:a", 600);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), null);
});

Deno.test("a write keeps entries another tab put in the same bucket", () => {
  const storage = memoryStorage({
    [ledgerStorageKey("u-1")]: '{"room:other":9000}',
  });
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  assertEquals(JSON.parse(storage.getItem(ledgerStorageKey("u-1"))!), {
    "room:other": 9000,
    "room:a": 500,
  });
});

Deno.test("notes taken while signed out stay in memory", () => {
  const storage = memoryStorage();
  const store = new RecentStore(storage);
  store.note("room:a", 500);
  assertEquals(store.at("room:a"), 500);
  assertEquals(storage.getItem(ledgerStorageKey("u-1")), null);
});

Deno.test("a degraded store still removes the bucket on disk", () => {
  // Over-quota storage: writes throw, removals do not. Signing out must still
  // clear the bucket, or the last account's ordering stays on the browser for
  // the next person.
  const removed: string[] = [];
  const storage: RecentStorage = {
    getItem: () => null,
    setItem: () => {
      throw new DOMException("quota", "QuotaExceededError");
    },
    removeItem: (key) => void removed.push(key),
  };
  const store = new RecentStore(storage);
  store.useUser("u-1");
  store.note("room:a", 500);
  // The note is kept for this session even though it could not be written.
  assertEquals(store.at("room:a"), 500);

  store.reset();
  assertEquals(removed, [ledgerStorageKey("u-1")]);
});
