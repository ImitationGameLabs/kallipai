// Behaviour tests for a room's session: the lease that keeps the room entered
// while any surface still draws it, and the cursor a leaving surface may or may
// not report. The module is rune-bearing and `deno test` runs it uncompiled, so
// it loads behind the same passthrough $state/$derived shim the window's test
// uses; the stores it commands are stubbed, because what is under test is the
// order and the arguments of those commands, not the stores' own payloads.

import { assertEquals } from "@std/assert";
import type { RoomConversation } from "../session/roomConversations.svelte.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  function $derived<T>(fn: () => T): T;
}

const shims = globalThis as Record<string, unknown>;
shims["$state"] = (v: unknown) => v;
// `$derived(expression)` in the source, not a thunk: Svelte's compiler is what
// wraps it, and `deno test` runs the file as written, so the shim hands the
// value straight back. A derived cell is therefore a snapshot at factory time
// here -- which is why the cases below read the window's own state, never a
// computed field.
const derived = (value: unknown) => value;
Object.assign(derived, { by: (fn: () => unknown) => fn() });
shims["$derived"] = derived;

const { roomConversationsStore } =
  await import("../session/roomConversations.svelte.ts");
const { unreadStore } = await import("../session/unread.svelte.ts");
const { createRoomSession } = await import("./roomSession.svelte.ts");

/** What the session told the stores, in order. */
const told: string[] = [];

/** The room's own state, as the session reads it: no network, no page. */
function stubStores(lastSeq: number | null): void {
  roomConversationsStore.open = () => Promise.resolve();
  roomConversationsStore.refresh = () => Promise.resolve();
  roomConversationsStore.refreshRoster = () => Promise.resolve();
  roomConversationsStore.get = () =>
    lastSeq === null ? undefined : ({ lastSeq } as RoomConversation);
  unreadStore.enter = (key: string) => {
    told.push(`enter ${key}`);
  };
  unreadStore.noteViewedLines = (roomId: string, seq: number | null) => {
    told.push(`note ${roomId} ${seq ?? "-"}`);
  };
  unreadStore.leaveRoom = (roomId: string, seq?: number | null) => {
    told.push(`leave ${roomId} ${seq ?? "-"}`);
  };
}

Deno.test("the room stays entered until the last surface leaves", () => {
  told.length = 0;
  stubStores(7);
  const room = createRoomSession(() => "room-1");
  const discussion = createRoomSession(() => "room-1", {
    advanceOnLeave: false,
  });

  room.sync();
  discussion.sync();
  assertEquals(told, ["enter room:room-1"], "one enter for two surfaces");

  room.stop();
  assertEquals(told, ["enter room:room-1", "note room-1 7"]);
  assertEquals(
    told.filter((line) => line.startsWith("leave")).length,
    0,
    "a surface that leaves while another holds the room does not end viewing",
  );

  discussion.stop();
  assertEquals(told, ["enter room:room-1", "note room-1 7", "leave room-1 -"]);
});

Deno.test(
  "a session that advances on leave reports the cursor it last saw",
  () => {
    told.length = 0;
    stubStores(7);
    const room = createRoomSession(() => "room-1");

    room.sync();
    room.stop();
    assertEquals(told, ["enter room:room-1", "leave room-1 7"]);
  },
);
