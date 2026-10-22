// Tests for the top-prepend scroll anchor controller. The transcript restores
// the reading position by the height delta a page adds above the viewport;
// the snapshot therefore describes ONE transcript's geometry, and a page still
// in flight when the session moves must not move the next one. These cases pin
// that fence directly on the controller (no DOM beyond a stand-in viewport).

import { assertEquals } from "@std/assert";
import { createScrollAnchor } from "./transcript.svelte.ts";

interface Box {
  scrollTop: number;
  scrollHeight: number;
}

function box(scrollTop: number, scrollHeight: number): Box {
  return { scrollTop, scrollHeight };
}

function anchorFor(
  viewport: { current: Box | undefined },
  scope: { current: string | undefined },
) {
  return createScrollAnchor(
    () => viewport.current as unknown as HTMLDivElement | undefined,
    () => scope.current,
  );
}

Deno.test("a landed page restores the position by its height delta", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "room-1" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  assertEquals(anchor.pending, true);

  // Ten older lines land above the viewport: 200px of new content.
  vp.current.scrollHeight = 1200;
  assertEquals(anchor.consume(30), true);
  assertEquals(vp.current.scrollTop, 300);
  assertEquals(anchor.pending, false);
});

Deno.test("an unscoped surface keeps the plain capture-and-restore", () => {
  const vp = { current: box(40, 500) };
  const scope: { current: string | undefined } = { current: undefined };
  const anchor = anchorFor(vp, scope);

  anchor.capture(8);
  vp.current.scrollHeight = 640;
  assertEquals(anchor.consume(12), true);
  assertEquals(vp.current.scrollTop, 180);
});

Deno.test("a snapshot never moves the session that replaced it", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  // The reader switches session while the page is still in flight.
  scope.current = "session-b";
  vp.current = box(0, 400);

  assertEquals(anchor.consume(30), false);
  assertEquals(vp.current.scrollTop, 0);
  assertEquals(anchor.pending, false);
});

Deno.test("a stale snapshot is dropped while its page is still loading", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  scope.current = "session-b";
  anchor.release(20, true);
  assertEquals(anchor.pending, false);

  // Coming back does not resurrect it: it described the other transcript.
  scope.current = "session-a";
  vp.current.scrollHeight = 1200;
  assertEquals(anchor.consume(30), false);
  assertEquals(vp.current.scrollTop, 100);
});

Deno.test(
  "a second capture in the same session keeps the first snapshot",
  () => {
    const vp = { current: box(100, 1000) };
    const scope = { current: "session-a" };
    const anchor = anchorFor(vp, scope);

    anchor.capture(20);
    vp.current.scrollTop = 150;
    vp.current.scrollHeight = 1010;
    anchor.capture(20);

    vp.current.scrollHeight = 1200;
    anchor.consume(30);
    assertEquals(vp.current.scrollTop, 300);
  },
);

Deno.test("a capture in a new session replaces the old baseline", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  scope.current = "session-b";
  vp.current = box(10, 300);
  anchor.capture(4);

  vp.current.scrollHeight = 400;
  assertEquals(anchor.consume(9), true);
  assertEquals(vp.current.scrollTop, 110);
});

Deno.test("a page that added no height consumes without moving", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  assertEquals(anchor.consume(25), true);
  assertEquals(vp.current.scrollTop, 100);
});

Deno.test("release drops only a settled page that added nothing", () => {
  const vp = { current: box(100, 1000) };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  anchor.release(20, true);
  assertEquals(anchor.pending, true);

  anchor.release(30, false);
  assertEquals(anchor.pending, true);

  anchor.release(20, false);
  assertEquals(anchor.pending, false);
});

Deno.test("without a viewport nothing is captured", () => {
  const vp: { current: Box | undefined } = { current: undefined };
  const scope = { current: "session-a" };
  const anchor = anchorFor(vp, scope);

  anchor.capture(20);
  assertEquals(anchor.pending, false);
  assertEquals(anchor.consume(30), false);
});
