// Behavior test for the store send gate's offline leg: the gate judges
// status ("open" | "offline"), not transport presence, so the offline
// view's designed send path -- optimistic line, durable pending row,
// sendFailed -- is reachable from the store. A gate that judged
// transport presence instead would drop such a send silently: the
// offline view has no transport at all.
//
// Seams follow the channels_refresh_test pattern: the modules under test are
// rune-bearing but `deno test` runs them uncompiled, so a passthrough $state
// shim lets them run with plain fields; the conversation is spliced into the
// private conversations map via a structural cast. putPending lands in the
// no-IDB guard (the cache helpers swallow the absence in tests).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type ChannelsStore = import("./channels.svelte.ts").ChannelsStore;
type OfflineConversation =
  import("./conversation.svelte.ts").OfflineConversation;
const { assertEquals } = await import("@std/assert");
const { ChannelsStore } = await import("./channels.svelte.ts");
const { OfflineConversation } = await import("./conversation.svelte.ts");

function storeWith(conv: OfflineConversation): ChannelsStore {
  const store = new ChannelsStore();
  (
    store as unknown as { conversations: Map<string, OfflineConversation> }
  ).conversations.set("conv-off", conv);
  return store;
}

function offlineConv(): OfflineConversation {
  return new OfflineConversation(
    "conv-off",
    { get: () => undefined },
    "tagma-1",
    { kind: "user", id: "u-1", handle: "alice" },
  );
}

Deno.test(
  "the store gate routes an offline conversation's send to the failed-line path",
  () => {
    const store = storeWith(offlineConv());
    const conv = (
      store as unknown as { conversations: Map<string, OfflineConversation> }
    ).conversations.get("conv-off")!;

    assertEquals(conv.transcript.lines.length, 0, "starts empty");
    store.send("conv-off", "hello offline");

    assertEquals(conv.transcript.lines.length, 1, "the line renders");
    assertEquals(conv.transcript.lines[0].status, "failed");
    assertEquals(conv.transcript.lines[0].text, "hello offline");
  },
);

Deno.test(
  "the store gate still refuses a conversation that is neither open nor offline",
  () => {
    const store = storeWith(offlineConv());
    const conv = (
      store as unknown as { conversations: Map<string, OfflineConversation> }
    ).conversations.get("conv-off")!;
    conv.status = "opening";

    store.send("conv-off", "too early");
    assertEquals(
      conv.transcript.lines.length,
      0,
      "a mid-handshake conversation takes no sends",
    );
  },
);
