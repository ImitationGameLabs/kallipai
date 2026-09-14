// Left-edge swipe detector: unit legs over the real window EventTarget,
// dispatching synthetic touch triples. Covers the four decisions the
// detector makes: in-edge horizontal opens, out-of-edge and short drags
// are inert, vertical-dominant drags bail, and the detacher unhooks.

import { assertEquals } from "@std/assert";
import { attachEdgeSwipe } from "./edgeSwipe.ts";

function touch(type: string, x: number, y: number): Event {
  const e = new Event(type);
  Object.defineProperty(e, "touches", {
    value: [{ clientX: x, clientY: y }],
  });
  return e;
}

function swipe(points: Array<[number, number]>): void {
  const [sx, sy] = points[0];
  dispatchEvent(touch("touchstart", sx, sy));
  for (const [x, y] of points.slice(1)) {
    dispatchEvent(touch("touchmove", x, y));
  }
  dispatchEvent(touch("touchend", 0, 0));
}

Deno.test("edgeSwipe: in-edge horizontal drag past threshold opens", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  swipe([
    [10, 300],
    [40, 300],
    [70, 300],
  ]);
  detach();
  assertEquals(opened, 1);
});

Deno.test("edgeSwipe: start outside the edge band is inert", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  swipe([
    [40, 300],
    [120, 300],
  ]);
  detach();
  assertEquals(opened, 0);
});

Deno.test("edgeSwipe: short horizontal drag under threshold is inert", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  swipe([
    [10, 300],
    [40, 300],
  ]);
  detach();
  assertEquals(opened, 0);
});

Deno.test("edgeSwipe: vertical-dominant drag bails and cannot revive", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  swipe([
    [10, 300],
    [20, 380],
    [80, 380],
  ]);
  detach();
  assertEquals(opened, 0);
});

Deno.test(
  "edgeSwipe: touchcancel clears the stale start (no mid-screen revival)",
  () => {
    let opened = 0;
    const detach = attachEdgeSwipe(() => (opened += 1));
    // An in-edge gesture begins, then the system takes over (a call, a
    // notification sheet) and cancels it. The next gesture starts
    // mid-screen: without the cancel reset, the stale start point would
    // make this read as a wide horizontal drag and open the drawer.
    dispatchEvent(touch("touchstart", 10, 300));
    dispatchEvent(touch("touchmove", 30, 300));
    dispatchEvent(touch("touchcancel", 0, 0));
    swipe([
      [200, 300],
      [280, 300],
    ]);
    detach();
    assertEquals(opened, 0);
  },
);

Deno.test("edgeSwipe: moves after a cancel do not open late", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  dispatchEvent(touch("touchstart", 10, 300));
  dispatchEvent(touch("touchcancel", 0, 0));
  dispatchEvent(touch("touchmove", 80, 300));
  dispatchEvent(touch("touchend", 0, 0));
  detach();
  assertEquals(opened, 0);
});

Deno.test("edgeSwipe: detacher unhooks the listeners", () => {
  let opened = 0;
  const detach = attachEdgeSwipe(() => (opened += 1));
  detach();
  swipe([
    [10, 300],
    [70, 300],
  ]);
  assertEquals(opened, 0);
});
