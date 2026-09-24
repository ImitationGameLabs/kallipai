// Tests for the automatic-open failure budget: cooldown-gated retries with
// doubling backoff, the session terminal after six straight failures, the
// the user-driven re-arm path (an explicit open bypasses and clears the
// budget), success clearing the failure memory, and the offline-class
// (503) failure asking the shell to correct stale presence.
//
// Seams, and why they are safe: the module under test is rune-bearing, but
// `deno test` runs it uncompiled, so a passthrough $state shim (the
// archeionMint_test / relayWindow_test pattern) lets the store run with plain
// fields. `openRelay` is overridden in a Harness subclass -- ensureOpen's
// budget contract is with openRelay (attempt / fail / succeed), not with the
// KEX stack under it, so stubbing there tests exactly the retry policy. The
// clock is the store's own `now()` seam, so no fake timers are needed:
// cooldown boundaries are asserted by moving the fake clock across them.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type TagmaView = import("@kallipai/kallip-archeion-client").TagmaView;
const { LescheApiError } = await import("@kallipai/kallip-lesche-client");
const { assertEquals } = await import("@std/assert");
const { ChannelsStore } = await import("./channels.svelte.ts");

type Step = Error | "ok";

/** Scripted openRelay: each call shifts the next step; counts attempts. */
class Harness extends ChannelsStore {
  attempts = 0;
  script: Step[] = [];
  clock = 1_000_000;
  corrections: string[] = [];

  constructor() {
    super();
    this.setOfflineCorrection((tagmaId) => this.corrections.push(tagmaId));
  }

  override async openRelay(_tagma: TagmaView): Promise<string> {
    this.attempts++;
    const step = this.script.shift();
    if (step === undefined) throw new Error("script exhausted");
    if (step === "ok") return "conv-1";
    throw step;
  }

  protected override now(): number {
    return this.clock;
  }
}

const tagma = { tagma_id: "t-1", state: "enrolled" } as unknown as TagmaView;

Deno.test(
  "auto-open backs off and terminals after six straight failures",
  async () => {
    const h = new Harness();
    h.script = Array.from({ length: 6 }, () => new Error("net down"));
    // Six failures, each after its cooldown expires (1s, 2s, 4s, 8s, 16s, 32s).
    for (let i = 0; i < 6; i++) {
      await h.ensureOpen(tagma);
      assertEquals(h.attempts, i + 1);
      // Inside the cooldown an immediate automatic retry is refused.
      h.clock += 100;
      await h.ensureOpen(tagma);
      assertEquals(h.attempts, i + 1);
      h.clock += 60_000;
    }
    assertEquals(h.isAutoOpenFailed("t-1"), true);
    // Terminal: no further automatic attempts, even past any cooldown.
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 6);
  },
);

Deno.test(
  "an explicit open bypasses the cooldown and success clears the budget",
  async () => {
    const h = new Harness();
    h.script = [new Error("net down"), "ok", "ok"];
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 1);
    assertEquals(h.isAutoOpenFailed("t-1"), true);
    // Still inside the 1s cooldown, but a user-driven open goes through;
    // its SUCCESS clears the budget.
    await h.ensureOpen(tagma, { explicit: true });
    assertEquals(h.attempts, 2);
    assertEquals(h.isAutoOpenFailed("t-1"), false);
    h.clock += 60_000;
    // An explicit FAILURE still counts (only success clears): the next
    // automatic open inside the new cooldown is refused.
    h.script = [new Error("net down")];
    await h.ensureOpen(tagma, { explicit: true });
    assertEquals(h.attempts, 3);
    h.clock += 100;
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 3);
    // The budget restarted from this single failure (the earlier
    // success cleared it), so the fresh 1s cooldown still refuses
    // an immediate automatic retry.
    await h.ensureOpen(tagma);
    assertEquals(h.attempts, 3);
  },
);

Deno.test(
  "an offline-class failure asks the shell to correct stale presence",
  async () => {
    const h = new Harness();
    h.script = [new LescheApiError(503, "tagma offline")];
    await h.ensureOpen(tagma);
    assertEquals(h.corrections, ["t-1"]);

    // A non-offline failure must NOT retract presence (the tagma may be fine;
    // the lesche just failed to serve the KEX).
    h.script = [new Error("net down")];
    await h.ensureOpen(tagma);
    assertEquals(h.corrections.length, 1);
  },
);

Deno.test(
  "tearDownAll clears the failure budget (it is session-scoped)",
  async () => {
    const h = new Harness();
    h.script = [new Error("net down")];
    await h.ensureOpen(tagma);
    assertEquals(h.isAutoOpenFailed("t-1"), true);
    h.tearDownAll();
    assertEquals(h.isAutoOpenFailed("t-1"), false);
  },
);

Deno.test(
  "getTagmaChannelState derives unavailable from a recorded failure",
  async () => {
    const h = new Harness();
    // Before any attempt: no conversation, no budget entry -> absent (the
    // auto path is about to try, so the spinner is honest).
    assertEquals(h.getTagmaChannelState("t-1").kind, "absent");
    h.script = [new Error("net down")];
    await h.ensureOpen(tagma);
    // Budget entry, no conversation, nothing in flight: the settled dot,
    // matching the chat page's unavailable + retry row (same source).
    assertEquals(h.getTagmaChannelState("t-1").kind, "unavailable");
    // A successful explicit retry clears the budget; the stub never
    // inserts a conversation, so the state settles back to absent.
    h.script = ["ok"];
    await h.ensureOpen(tagma, { explicit: true });
    assertEquals(h.getTagmaChannelState("t-1").kind, "absent");
  },
);

Deno.test(
  "a terminal budget blocks even the refresh leg (presence transitions cannot resync an offline peer)",
  async () => {
    const h = new Harness();
    h.script = Array.from(
      { length: 6 },
      () => new LescheApiError(503, "tagma offline"),
    );
    // Six 503 failures terminal the budget...
    for (let i = 0; i < 6; i++) {
      await h.ensureOpen(tagma);
      h.clock += 60_000;
    }
    assertEquals(h.attempts, 6);
    // ...and a presence online-transition refresh (NOT explicit) is refused
    // past the terminal -- this is the leg the presence sink drives on every
    // snapshot the SSE replays. If this leaked, the 503 loop would never end.
    await h.ensureOpen(tagma, { refresh: true });
    assertEquals(h.attempts, 6);
  },
);
