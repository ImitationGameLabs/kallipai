import { assertEquals } from "@std/assert";
import { tagmaForConversation } from "./convOf.ts";

// localStorage stub: a fresh stub object per case, installed/restored around
// each test. `blocked` throws on access, exercising the storage-blocked path.
function stubStorage(entries: Record<string, string>, blocked = false): void {
  const real = globalThis.localStorage;
  const store = new Map(Object.entries(entries));
  const proxy = {
    getItem: (k: string) => {
      if (blocked) throw new Error("storage blocked");
      return store.has(k) ? store.get(k)! : null;
    },
    setItem: (k: string, v: string) => {
      if (blocked) throw new Error("storage blocked");
      store.set(k, v);
    },
    key: (i: number) => {
      if (blocked) throw new Error("storage blocked");
      return [...store.keys()][i] ?? null;
    },
    get length() {
      if (blocked) throw new Error("storage blocked");
      return store.size;
    },
  };
  Object.defineProperty(globalThis, "localStorage", {
    value: proxy,
    configurable: true,
  });
  void real;
}

Deno.test("tagmaForConversation: hit resolves the owning tagma", () => {
  stubStorage({ "kallip-relay:conv-of:t1": "conv-1" });
  try {
    assertEquals(tagmaForConversation("conv-1"), "t1");
  } finally {
    stubStorage({});
  }
});

Deno.test("tagmaForConversation: miss returns undefined", () => {
  stubStorage({ "kallip-relay:conv-of:t1": "conv-1" });
  try {
    assertEquals(tagmaForConversation("conv-other"), undefined);
  } finally {
    stubStorage({});
  }
});

Deno.test("tagmaForConversation: blocked storage returns undefined", () => {
  stubStorage({}, true);
  try {
    assertEquals(tagmaForConversation("conv-1"), undefined);
  } finally {
    stubStorage({});
  }
});
