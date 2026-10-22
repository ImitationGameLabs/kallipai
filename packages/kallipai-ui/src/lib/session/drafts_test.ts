// Tests for the drafts store: round-trip through both layers, the
// empty-string-is-delete contract, seeding from storage on construction, and
// reset's prefix scan. Storage is stubbed with a Map so no real
// sessionStorage is touched (Deno has none).

import { assertEquals } from "@std/assert";
import type { DraftStorage } from "./drafts.ts";
import {
  ChatDraftsStore,
  conversationStorageKey,
  restoreFailedDraft,
} from "./drafts.ts";

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

Deno.test("a failed send stores its text under the key that sent it", () => {
  const store = new ChatDraftsStore(memoryStorage());
  const shown: string[] = [];
  const sending = { key: "r:room-a", restore: () => {} };
  const live = { key: "r:room-b", restore: () => {} };
  restoreFailedDraft({
    sendingKey: "r:room-a",
    sendingBinding: sending,
    liveBinding: live,
    text: "half typed",
    store,
    show: (text) => shown.push(text),
  });
  assertEquals(store.get("r:room-a"), "half typed");
  assertEquals(store.get("r:room-b"), "");
  assertEquals(shown, []);
});

Deno.test("a failed send repaints the field while its binding holds", () => {
  const store = new ChatDraftsStore(memoryStorage());
  const shown: string[] = [];
  const sending = { key: "r:room-a", restore: () => {} };
  restoreFailedDraft({
    sendingKey: "r:room-a",
    sendingBinding: sending,
    liveBinding: sending,
    text: "half typed",
    store,
    show: (text) => shown.push(text),
  });
  assertEquals(store.get("r:room-a"), "half typed");
  assertEquals(shown, ["half typed"]);
});

Deno.test("a return to the same conversation keeps its new input", () => {
  const store = new ChatDraftsStore(memoryStorage());
  const shown: string[] = [];
  const sentFrom = { key: "r:room-a", restore: () => {} };
  // Back on the same conversation, through a binding installed later: the
  // key matches and the object does not, so the new input stays on screen.
  const live = { key: "r:room-a", restore: () => {} };
  restoreFailedDraft({
    sendingKey: "r:room-a",
    sendingBinding: sentFrom,
    liveBinding: live,
    text: "half typed",
    store,
    show: (text) => shown.push(text),
  });
  assertEquals(store.get("r:room-a"), "half typed");
  assertEquals(shown, []);
});

Deno.test("a relay conversation keys on its tagma, else on its id", () => {
  assertEquals(conversationStorageKey("conv-1", "tagma-1"), "t:tagma-1");
  assertEquals(conversationStorageKey("conv-1", undefined), "c:conv-1");
  assertEquals(conversationStorageKey("local", undefined), "c:local");
});
