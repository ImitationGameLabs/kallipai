// Tests for the headless auto-scroll controller's miss counting. The counting
// fence is extracted as the pure missedLines() so the prepend-vs-append
// distinction is testable without a viewport (the badgeLabel precedent in
// session/unread.svelte.ts); the controller's own runes wiring has no
// runtime test harness here (the composer_test rationale), so it gets
// source-read pins instead.

import { assert, assertEquals } from "@std/assert";
import { missedLines, type TailSnapshot } from "./transcript.svelte.ts";

const MODULE = new URL("./transcript.svelte.ts", import.meta.url);
const CV = new URL("../components/ConversationView.svelte", import.meta.url);
const ROOM = new URL("../pages/RoomConversationPage.svelte", import.meta.url);
const CHANNEL = new URL("../pages/ChannelChatPage.svelte", import.meta.url);
const DIRECT = new URL("../pages/DirectSessionPage.svelte", import.meta.url);
const COMPOSER = new URL("../components/Composer.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

const tail = (length: number, key?: string | number): TailSnapshot => ({
  length,
  key,
});

Deno.test("missedLines counts appended lines while detached", () => {
  assertEquals(missedLines(tail(5, 10), tail(8, 13), false), 3);
});

Deno.test("missedLines ignores a grown array whose tail did not move", () => {
  // A history prepend pages older lines in above the tail: the array grows
  // but the last line is unchanged, so nothing counts as new.
  assertEquals(missedLines(tail(5, 10), tail(105, 10), false), 0);
});

Deno.test("missedLines counts nothing while following", () => {
  assertEquals(missedLines(tail(5, 10), tail(9, 14), true), 0);
});

Deno.test(
  "missedLines counts nothing when content shrinks or stands still",
  () => {
    assertEquals(missedLines(tail(8, 13), tail(5, 10), false), 0);
    assertEquals(missedLines(tail(8, 13), tail(8, 13), false), 0);
  },
);

Deno.test(
  "the controller wires the fence, the reset, and the reactive count",
  { permissions: { read: [MODULE] } },
  () => {
    const src = source(MODULE);
    // stick() counts only through the missedLines fence (the prepend
    // guard), and the count drops when follow returns by hand or by
    // forceBottom.
    assert(src.includes("missed += missedLines("));
    assert(src.includes("if (follow) missed = 0;"));
    const fb = src.indexOf("function forceBottom");
    assert(fb >= 0, "forceBottom must exist");
    const block = src.slice(fb, src.indexOf("}", fb));
    assert(block.includes("follow = true;"));
    assert(block.includes("missed = 0;"));
    // The count is exposed reactively and the action is returned.
    assert(src.includes("get missed()"));
    assert(src.includes("forceBottom,"));
  },
);

Deno.test(
  "reset restores fresh-mount state and keeps the viewport binding",
  { permissions: { read: [MODULE] } },
  () => {
    const src = source(MODULE);
    const rb = src.indexOf("function reset");
    assert(rb >= 0, "reset must exist");
    // The reset block ends at the next controller function
    // (reanchorIfFollowing); slicing to `return {` would swallow it
    // and false-trip the viewport-freedom assertion below.
    const block = src.slice(
      rb,
      src.indexOf("function reanchorIfFollowing", rb),
    );
    assert(block.includes("follow = true;"));
    assert(block.includes("missed = 0;"));
    assert(block.includes("lastTail = { length: 0, key: undefined }"));
    // The viewport binding survives a reset: no re-bind, no scroll write.
    assert(!block.includes("viewport"));
  },
);

Deno.test(
  "both mount points reset on session identity changes",
  { permissions: { read: [MODULE, CV, ROOM, CHANNEL, DIRECT] } },
  () => {
    // The shared render body resets when the feeding session changes...
    const cv = source(CV);
    assert(cv.includes("void sessionKey;"));
    assert(cv.includes("scroll.reset()"));
    // ...the channel-like callers hand it the route identity...
    assert(source(CHANNEL).includes("sessionKey={conversationId}"));
    assert(source(DIRECT).includes("${tagmaId}:${peerId}"));
    // ...and the room page keys on its own room param.
    const room = source(ROOM);
    assert(room.includes("void roomId;"));
    assert(room.includes("scroll.reset()"));
  },
);

Deno.test(
  "the session-key reset is declared before the stick effect",
  { permissions: { read: [CV, ROOM] } },
  () => {
    for (const src of [source(CV), source(ROOM)]) {
      const resetAt = src.indexOf("scroll.reset()");
      const stickAt = src.indexOf("scroll.stick(");
      assert(resetAt >= 0 && stickAt >= 0);
      // Effects in the same flush run in declaration order: the reset
      // must be declared first or stick() observes a stale session.
      assert(
        resetAt < stickAt,
        "reset effect must be declared before the stick effect",
      );
    }
  },
);

Deno.test(
  "the submit hook fires in the synchronous segment (before the transport await)",
  { permissions: { read: [MODULE, CV, ROOM, COMPOSER] } },
  () => {
    const cs = source(COMPOSER);
    // An actual send returns the transcript to the
    // tail. Both trigger sites (Enter key, send button) route through the
    // single wrapper, and the hook fires before submit() -- i.e. before
    // the internal transport await. A call ordered after it would scroll
    // a conversation switched to during that await (the switch runs
    // reset()); this ordering pins the synchronous-segment contract.
    const hook = cs.indexOf("onSubmitted?.();");
    const sub = cs.indexOf("composer.submit();");
    assert(hook >= 0, "the wrapper must fire onSubmitted");
    assert(sub > hook, "onSubmitted must precede composer.submit()");
    assertEquals(
      (cs.match(/composer\.submit\(\)/g) ?? []).length,
      1,
      "submit() is only ever reached through the wrapper",
    );
    // Both hosts wire the hook to their own auto-scroll controller.
    assert(source(CV).includes("onSubmitted={() => scroll.forceBottom()}"));
    assert(source(ROOM).includes("onSubmitted={() => scroll.forceBottom()}"));
  },
);

Deno.test(
  "forceBottom serves a scrolled-up send; a later reset re-arms",
  async () => {
    // Passthrough $state shim (the statusCard_test pattern): the controller's
    // value semantics hold without a reactive runtime.
    (globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
    const { createAutoScroll } = await import("./transcript.svelte.ts");
    const scroll = createAutoScroll();
    let top = 400;
    scroll.viewport = {
      get scrollTop() {
        return top;
      },
      set scrollTop(v: number) {
        top = v;
      },
      scrollHeight: 1400,
      clientHeight: 1000,
    } as HTMLDivElement;
    // Scrolled up reading history: detached; the first stick counts the
    // whole window (fresh-mount tail: nothing was ever seen).
    scroll.onScroll();
    top = 100;
    scroll.onScroll();
    scroll.stick(13, 13);
    assertEquals(scroll.follow, false);
    assertEquals(scroll.missed, 13);
    // The send hook: back to the tail, count cleared, viewport pinned.
    scroll.forceBottom();
    assertEquals(scroll.follow, true);
    assertEquals(scroll.missed, 0);
    assertEquals(top, 1400);
    // A session switch inside the transport await resets fresh; no late
    // call exists to drag the new conversation (ordering pinned above).
    scroll.reset();
    assertEquals(scroll.follow, true);
    assertEquals(scroll.missed, 0);
  },
);

Deno.test(
  "observe re-anchors a following transcript on resize; inert detached",
  async () => {
    // The layout bug: a growing composer shrinks the scroll container
    // without firing a scroll event, so a following transcript drifts
    // off the tail and the next wheel tick reads as the user leaving.
    // The observer must re-anchor while following, stay inert while
    // detached (user intent wins), and die with its detacher.
    class FakeResizeObserver {
      static instances: FakeResizeObserver[] = [];
      #dead = false;
      constructor(private cb: () => void) {
        FakeResizeObserver.instances.push(this);
      }
      observe(_target: Element): void {}
      disconnect(): void {
        this.#dead = true;
      }
      fire(): void {
        if (!this.#dead) this.cb();
      }
    }
    (globalThis as Record<string, unknown>)["ResizeObserver"] =
      FakeResizeObserver;
    (globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
    const { createAutoScroll } = await import("./transcript.svelte.ts");
    const scroll = createAutoScroll();
    let top = 400;
    const vp = {
      get scrollTop() {
        return top;
      },
      set scrollTop(v: number) {
        top = v;
      },
      scrollHeight: 1400,
      clientHeight: 1000,
    } as HTMLDivElement & { scrollHeight: number };
    scroll.viewport = vp;
    const detach = scroll.observe(vp);
    // Following: a container resize re-anchors to the tail.
    FakeResizeObserver.instances[0].fire();
    assertEquals(top, 1400);
    // Scrolled up: the same resize leaves the reading position alone.
    top = 100;
    scroll.onScroll();
    FakeResizeObserver.instances[0].fire();
    assertEquals(top, 100);
    // The detacher kills the observer: a resize after the detach must
    // not re-anchor even while following (the tail moved on to 2000;
    // a live observer would drag scrollTop there, a dead one cannot).
    detach();
    scroll.forceBottom();
    vp.scrollHeight = 2000;
    FakeResizeObserver.instances[0].fire();
    assertEquals(top, 1400);
  },
);

Deno.test(
  "both hosts attach the resize observer to their own viewport",
  { permissions: { read: [MODULE, CV, ROOM] } },
  () => {
    const src = source(MODULE);
    // The observer re-anchors only while following; detached it is inert.
    const ob = src.indexOf("function observe");
    assert(ob >= 0, "observe must exist");
    const block = src.slice(ob, src.indexOf("return {", ob));
    assert(block.includes("new ResizeObserver(reanchorIfFollowing)"));
    // The follow guard lives in the shared helper the RO delegates to.
    const guard = src.indexOf("function reanchorIfFollowing");
    const gblock = src.slice(guard, src.indexOf("}", guard));
    assert(gblock.includes("if (viewport && follow)"));
    assert(gblock.includes("viewport.scrollTop = viewport.scrollHeight"));
    // Each host binds the observer to its own scroll container and keeps
    // the detacher as the effect cleanup.
    for (const host of [source(CV), source(ROOM)]) {
      assert(host.includes("return scroll.observe(vp);"));
    }
  },
);

Deno.test(
  "the submit wrapper fires the hook only on an actual send",
  { permissions: { read: [COMPOSER] } },
  () => {
    const cs = source(COMPOSER);
    // The wrapper applies the model's send gate -- the same one that
    // disables the button -- before the hook: a keystroke that would
    // leave the button disabled (empty draft, mid-flight send) is not
    // a send and must not drag the transcript to the tail.
    const wrap = cs.indexOf("function submitAtTail");
    assert(wrap >= 0, "the wrapper must exist");
    const guard = cs.indexOf(
      "if (!composer.canSend || composer.sending) return;",
      wrap,
    );
    assert(guard >= 0, "the wrapper must gate on the send gate");
    assert(
      guard < cs.indexOf("onSubmitted?.();", wrap),
      "the gate must precede the hook",
    );
  },
);

Deno.test(
  "the composer settle re-anchors a following transcript",
  { permissions: { read: [MODULE, CV, ROOM, COMPOSER] } },
  () => {
    const src = source(MODULE);
    // The composer's measure-collapse can clamp a bottom-pinned
    // scrollTop against the transient layout; the queued scroll event
    // then reads the settled geometry as a user departure. The settle
    // hook routes back into the controller, which re-anchors while
    // following and stays inert while detached.
    assert(src.includes("new ResizeObserver(reanchorIfFollowing)"));
    const cs = source(COMPOSER);
    const settle = cs.indexOf("onResized?.();");
    const setHeight = cs.indexOf("area.style.height");
    const settledHeight = cs.indexOf("area.style.height", setHeight + 1);
    assert(setHeight >= 0, "resize must collapse the height");
    assert(settledHeight > setHeight, "resize must settle the height");
    assert(settle > settledHeight, "the hook must fire after the settle");
    for (const host of [source(CV), source(ROOM)]) {
      assert(host.includes("onResized={() => scroll.reanchorIfFollowing()}"));
    }
  },
);

Deno.test("reanchorIfFollowing writes only while following", async () => {
  // Passthrough $state shim (the forceBottom test pattern).
  (globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
  const { createAutoScroll } = await import("./transcript.svelte.ts");
  const scroll = createAutoScroll();
  let top = 100;
  const vp = {
    get scrollTop() {
      return top;
    },
    set scrollTop(v: number) {
      top = v;
    },
    scrollHeight: 1400,
    clientHeight: 1000,
  } as HTMLDivElement;
  // Detached (dist 300): user intent wins, no write.
  scroll.viewport = vp;
  scroll.onScroll();
  assertEquals(top, 100);
  scroll.reanchorIfFollowing();
  assertEquals(top, 100);
  // No viewport: inert.
  scroll.forceBottom();
  scroll.viewport = undefined;
  top = 555;
  scroll.reanchorIfFollowing();
  assertEquals(top, 555);
  // Following: back to the tail.
  scroll.viewport = vp;
  scroll.reanchorIfFollowing();
  assertEquals(top, 1400);
});
