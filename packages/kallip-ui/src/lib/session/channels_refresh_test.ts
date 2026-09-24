// Tests for the epoch-refresh leg of ensureOpen: the presence sink's online
// transition passes `refresh: true` so an ALREADY-open channel is torn down
// (cache kept) and re-opened -- a restarted peer's fresh epoch cannot read
// the old session key, and the stale channel would silently drop sends.
//
// Seams, and why they are safe (the channels_openBudget_test pattern): the
// module under test is rune-bearing, but `deno test` runs it uncompiled, so
// a passthrough $state shim lets the store run with plain fields.
// `openRelay` is overridden in a Harness subclass: ensureOpen's contract is
// with openRelay (attempt / re-key / succeed), not the KEX stack under it.
// The override also registers a FakeEntry through the same maps the real
// openRelay's setRelayConv fills (private, so a structural cast -- the same
// discipline as relayWindow_test's transport splice), so the early-return
// path exercises a REAL findByTagma hit on an open conversation.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type TagmaView = import("@kallipai/kallip-archeion-client").TagmaView;
type Transport = import("./transport.ts").Transport;
const { assertEquals } = await import("@std/assert");
const { ChannelsStore } = await import("./channels.svelte.ts");
const { RelayConversation } = await import("./conversation.svelte.ts");

/** Minimal transport surface: the constructor reads localSender; close() is
 * the only method the teardown path calls. */
const fakeTransport: Transport = {
  localSender: { kind: "user", id: "u-1", handle: "alice" },
  async *replies() {},
  async *signals() {},
  async *status() {},
  send() {
    return Promise.resolve();
  },
  close() {},
} as unknown as Transport;

/** A stand-in relay conversation, inserted OPEN the way the real openRelay's
 * post-KEX registration does. Nothing beyond status/conversationId/close is
 * exercised by ensureOpen's refresh path. */
class FakeEntry extends RelayConversation {
  constructor(id: string, tagmaId: string) {
    super(id, null as never, fakeTransport, tagmaId, "T");
    this.status = "open";
  }
}

type Step = Error | "ok";

/** Scripted openRelay: each call shifts the next step; counts attempts and
 * registers a fake-open entry on "ok" (the budget clock is the store's own
 * now() seam, as in channels_openBudget_test). */
class Harness extends ChannelsStore {
  attempts = 0;
  script: Step[] = [];
  clock = 1_000_000;

  override openRelay(tagma: TagmaView): Promise<string> {
    this.attempts++;
    const step = this.script.shift();
    if (step === undefined) throw new Error("script exhausted");
    if (step instanceof Error) throw step;
    // Mirror the real openRelay's post-KEX sweep so the regression test
    // below exercises the same registration sequence as production.
    (
      this as unknown as { sweepOfflineViewsFor(id: string): void }
    ).sweepOfflineViewsFor(tagma.tagma_id);
    const conv = new FakeEntry("conv-1", tagma.tagma_id);
    const self = this as unknown as {
      conversations: Map<string, FakeEntry>;
      tagmaIndex: Map<string, string>;
    };
    self.conversations.set("conv-1", conv);
    self.tagmaIndex.set(tagma.tagma_id, "conv-1");
    return Promise.resolve("conv-1");
  }

  protected override now(): number {
    return this.clock;
  }
}

const tagma = { tagma_id: "t-1", state: "enrolled" } as unknown as TagmaView;

Deno.test(
  "a refresh re-keys an already-open channel; without it the open early-returns",
  async () => {
    const h = new Harness();
    h.script = ["ok", "ok"];
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 1);
    assertEquals(h.getTagmaChannelState("t-1").kind, "open");
    // Without the flag a settled-open channel early-returns: this is also
    // the offline leg's store-side contract -- the channel survives the
    // offline window, and the teardown rides the next ONLINE transition.
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 1);
    assertEquals(h.getTagmaChannelState("t-1").kind, "open");
    // With refresh the open channel is torn down (cache kept) and re-keyed:
    // a restarted peer cannot read the old session key.
    await h.ensureOpen(tagma, { refresh: true });
    assertEquals(h.attempts, 2);
    assertEquals(h.getTagmaChannelState("t-1").kind, "open");
  },
);

Deno.test(
  "a successful refresh clears the failure budget like any open",
  async () => {
    const h = new Harness();
    h.script = [new Error("net down"), "ok", "ok"];
    await h.ensureOpen(tagma); // fails once (cooldown + budget entry)
    assertEquals(h.isAutoOpenFailed("t-1"), true);
    h.clock += 60_000; // past any cooldown
    await h.ensureOpen(tagma); // succeeds -> budget cleared
    assertEquals(h.attempts, 2);
    assertEquals(h.isAutoOpenFailed("t-1"), false);
    await h.ensureOpen(tagma, { refresh: true }); // re-key succeeds
    assertEquals(h.attempts, 3);
    // The refresh rode the SAME success path, so the failure memory stays
    // cleared: an open channel implies the last open succeeded, and the
    // budget cannot be re-armed by a refresh.
    assertEquals(h.isAutoOpenFailed("t-1"), false);
  },
);

Deno.test(
  "a refresh on a dead channel is just the existing reopen (dead states never early-returned)",
  async () => {
    const h = new Harness();
    h.script = [new Error("net down"), "ok"];
    await h.ensureOpen(tagma); // fails -> unavailable
    assertEquals(h.getTagmaChannelState("t-1").kind, "unavailable");
    h.clock += 60_000;
    // The dead channel has no early-return to skip: with or without the
    // flag the path is the same tearDown-less reopen.
    await h.ensureOpen(tagma, { refresh: true });
    assertEquals(h.attempts, 2);
    assertEquals(h.getTagmaChannelState("t-1").kind, "open");
  },
);

Deno.test(
  "attachOfflineView mounts a synthetic view even with no remembered conversation",
  async () => {
    const h = new Harness();
    h.script = ["ok"];
    // attachOfflineView gates on a signed-in user (it stamps the sender);
    // save/restore keeps later tests in this file unaffected.
    const archeion = await import("./archeion.svelte.ts");
    const saved = archeion.archeionSession.user;
    archeion.archeionSession.user = {
      user_id: "u-1",
      username: "alice",
      display_name: "alice",
    } as never;
    try {
      // lastConversationOf reads localStorage, absent under deno test ->
      // undefined: the unified offline shape mounts the view anyway.
      await h.attachOfflineView("t-never");
      const view = h.offlineViewOf("t-never");
      if (!view) throw new Error("synthetic offline view must mount");
      assertEquals(view.conversationId, "offline-t-never");
      assertEquals(view.transcript.lines.length, 0);
      // Idempotent: a second call does not duplicate.
      await h.attachOfflineView("t-never");
      assertEquals(h.offlineViewOf("t-never"), view);
    } finally {
      archeion.archeionSession.user = saved;
    }
  },
);

Deno.test(
  "a successful open sweeps the offline view, synthetic key included",
  async () => {
    const h = new Harness();
    h.script = ["ok", "ok"];
    const archeion = await import("./archeion.svelte.ts");
    const saved = archeion.archeionSession.user;
    archeion.archeionSession.user = {
      user_id: "u-1",
      username: "alice",
      display_name: "alice",
    } as never;
    try {
      await h.attachOfflineView("t-sweep");
      if (!h.offlineViewOf("t-sweep")) {
        throw new Error("offline view must mount before the open");
      }
      const sweep = { tagma_id: "t-sweep", state: "enrolled" } as never;
      await h.ensureOpen(sweep);
      assertEquals(h.offlineViewOf("t-sweep"), undefined);
    } finally {
      archeion.archeionSession.user = saved;
    }
  },
);
