// The discussion page's Retry: a failed own line is sent again under the room,
// the discussion it belongs to and the row it answered. That promise is the
// argument tuple `resend` hands to `send`, so the tuple is what is asserted --
// drop `thread: line.thread` and the reply lands at the room's top level with
// every gate and every pin still green.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { roomConversationsStore } =
  await import("./roomConversations.svelte.ts");

/** A failed reply as the store holds it: an optimistic line, no server seq. */
const failed = {
  seq: -1,
  text: "again",
  mine: true,
  failed: true,
  thread: 1,
  quote: 5,
  mentions: ["p1"],
};

Deno.test("a failed reply is sent again in its own discussion", async () => {
  const store = roomConversationsStore as unknown as {
    convs: Map<string, { lines: readonly unknown[] }>;
    send: (room: string, text: string, options?: unknown) => Promise<void>;
  };
  const kept = { seq: 2, text: "kept", mine: false };
  const conv = { lines: [failed, kept] };
  store.convs.set("room-1", conv);
  const sent: unknown[] = [];
  store.send = (room, text, options) => {
    sent.push({ room, text, options });
    return Promise.resolve();
  };
  await roomConversationsStore.resend("room-1", failed as never);
  assertEquals(sent, [
    {
      room: "room-1",
      text: "again",
      options: { mentions: ["p1"], quote: 5, thread: 1 },
    },
  ]);
  assertEquals(
    conv.lines,
    [kept],
    "the line it replaces leaves with the resend",
  );
});
