// Tests for the drafts store: round-trip through both layers, the
// empty-string-is-delete contract, seeding from storage on construction, and
// reset's prefix scan. Storage is stubbed with a Map so no real
// sessionStorage is touched (Deno has none).

import { assertEquals } from "@std/assert";
import type { DraftStorage } from "./drafts.ts";
import { ChatDraftsStore } from "./drafts.ts";

function memoryStorage(seed?: Record<string, string>): DraftStorage {
  const data = new Map(Object.entries(seed ?? {}));
  return {
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
    removeItem: (k) => void data.delete(k),
    keys: () => [...data.keys()],
  };
}

Deno.test("set/get round-trips through map and storage", () => {
  const storage = memoryStorage();
  const store = new ChatDraftsStore(storage);
  store.set("t:tagma-1", "hello");
  assertEquals(store.get("t:tagma-1"), "hello");
  assertEquals(storage.getItem("kallipai.draft.t:tagma-1"), "hello");
  assertEquals(store.get("t:other"), "");
});

Deno.test("empty string deletes the entry from both layers", () => {
  const storage = memoryStorage();
  const store = new ChatDraftsStore(storage);
  store.set("r:room-1", "draft");
  store.set("r:room-1", "");
  assertEquals(store.get("r:room-1"), "");
  assertEquals(storage.getItem("kallipai.draft.r:room-1"), null);
});

Deno.test("construction seeds drafts from prefixed storage keys", () => {
  const storage = memoryStorage({
    "kallipai.draft.t:tagma-1": "survives reload",
    "kallipai.draft.c:local": "local draft",
    "unrelated.key": "foreign",
  });
  const store = new ChatDraftsStore(storage);
  assertEquals(store.get("t:tagma-1"), "survives reload");
  assertEquals(store.get("c:local"), "local draft");
});

Deno.test("reset drops only prefixed keys, keeping foreign entries", () => {
  const storage = memoryStorage({
    "kallipai.draft.t:tagma-1": "a",
    "unrelated.key": "keep me",
  });
  const store = new ChatDraftsStore(storage);
  store.set("r:room-1", "b");
  store.reset();
  assertEquals(store.get("t:tagma-1"), "");
  assertEquals(store.get("r:room-1"), "");
  assertEquals(storage.getItem("kallipai.draft.t:tagma-1"), null);
  assertEquals(storage.getItem("kallipai.draft.r:room-1"), null);
  assertEquals(storage.getItem("unrelated.key"), "keep me");
});
