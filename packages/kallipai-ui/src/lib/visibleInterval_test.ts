// Tests for the visibility gate on data polls: a hidden tab must stop the
// timer entirely (not merely slow it), the foreground return must re-arm it
// and fire once immediately, and stop() must drop the listener. The seam is
// the global `document`, which the module reads per call (never at import),
// so a minimal stub with a dispatchable `visibilitychange` drives the gate
// with no DOM. Real short intervals (20ms) follow the suite's
// no-fake-timers discipline (the channels_openBudget_test clock-seam pattern).

type Listener = () => void;

/** Minimal document stub: visibility state plus the one event the gate uses. */
class DocStub {
  visibilityState: "visible" | "hidden" = "visible";
  readonly listeners = new Set<Listener>();
  addEventListener(_type: string, fn: Listener): void {
    this.listeners.add(fn);
  }
  removeEventListener(_type: string, fn: Listener): void {
    this.listeners.delete(fn);
  }
  /** Transition and dispatch, as the browser would. */
  setVisibility(state: "visible" | "hidden"): void {
    this.visibilityState = state;
    for (const fn of this.listeners) fn();
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const { assertEquals } = await import("@std/assert");
const { startVisibleInterval } = await import("./visibleInterval.ts");

Deno.test("a hidden tab stops the poll timer entirely", async () => {
  const doc = new DocStub();
  (globalThis as Record<string, unknown>).document = doc;
  try {
    let ticks = 0;
    const stop = startVisibleInterval(() => ticks++, 20);
    doc.setVisibility("hidden");
    await sleep(70);
    assertEquals(ticks, 0);
    stop();
  } finally {
    delete (globalThis as Record<string, unknown>).document;
  }
});

Deno.test("foreground return re-arms and fires once immediately", async () => {
  const doc = new DocStub();
  (globalThis as Record<string, unknown>).document = doc;
  try {
    let ticks = 0;
    const stop = startVisibleInterval(() => ticks++, 20);
    doc.setVisibility("hidden");
    doc.setVisibility("visible");
    assertEquals(
      ticks,
      1,
      "foreground return fires at once, not one period later",
    );
    await sleep(70);
    assertEquals(ticks > 1, true, "periodic ticks resume after the return");
    stop();
    const frozen = ticks;
    await sleep(60);
    assertEquals(ticks, frozen, "stop() halts the timer");
    assertEquals(doc.listeners.size, 0, "stop() drops the visibility listener");
  } finally {
    delete (globalThis as Record<string, unknown>).document;
  }
});

Deno.test(
  "attached while hidden stays dormant until the first foreground",
  async () => {
    const doc = new DocStub();
    doc.visibilityState = "hidden";
    (globalThis as Record<string, unknown>).document = doc;
    try {
      let ticks = 0;
      const stop = startVisibleInterval(() => ticks++, 20);
      await sleep(50);
      assertEquals(ticks, 0, "no ticks while it started hidden");
      doc.setVisibility("visible");
      assertEquals(ticks, 1, "first foreground fires immediately");
      stop();
    } finally {
      delete (globalThis as Record<string, unknown>).document;
    }
  },
);
