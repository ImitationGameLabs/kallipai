// Left-edge swipe detector for the mobile agents drawer: a single
// horizontal drag starting within `edge` px of the viewport's left edge
// opens it. Deliberately minimal (no multi-touch, no velocity curves):
// one start/move/end/cancel set on window, passive listeners, and a
// vertical-dominant bail so a scroll that starts at the edge never
// fires. Returns its own detacher.
// The touch event is typed structurally (not the DOM
// TouchEvent) so this module type-checks under deno test,
// which has no DOM libs.
interface EdgeSwipeEvent extends Event {
  touches: Array<{ clientX: number; clientY: number }>;
}
export function attachEdgeSwipe(
  open: () => void,
  edge = 24,
  threshold = 48,
): () => void {
  let startX = 0;
  let startY = 0;
  let active = false;

  const onStart = (e: Event) => {
    const t = (e as EdgeSwipeEvent).touches[0];
    if (t && t.clientX <= edge) {
      startX = t.clientX;
      startY = t.clientY;
      active = true;
    }
  };
  const onMove = (e: Event) => {
    const t = (e as EdgeSwipeEvent).touches[0];
    if (!active) return;
    if (!t) return;
    const dx = t.clientX - startX;
    const dy = Math.abs(t.clientY - startY);
    if (dx > threshold && dx > dy) {
      active = false;
      open();
    } else if (dy > dx && dy > 24) {
      active = false;
    }
  };
  const onEnd = () => {
    active = false;
  };

  addEventListener("touchstart", onStart, { passive: true });
  addEventListener("touchmove", onMove, { passive: true });
  addEventListener("touchend", onEnd, { passive: true });
  // System takeover (an incoming call, a notification sheet) cancels the
  // gesture: the same reset as touchend, so a stale start point cannot
  // leak into the next gesture's dx.
  addEventListener("touchcancel", onEnd, { passive: true });
  return () => {
    removeEventListener("touchstart", onStart);
    removeEventListener("touchmove", onMove);
    removeEventListener("touchend", onEnd);
    removeEventListener("touchcancel", onEnd);
  };
}
