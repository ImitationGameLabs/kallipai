// Visibility-aware interval for data polls: while the tab is hidden the
// timer is stopped entirely (a backgrounded poll burns server quota for
// pixels nobody sees), and on the foreground return it re-arms and fires once
// immediately, because data frozen by the background freeze is exactly what
// the poll exists to prevent. Deliberately leaves event streams alone:
// pausing a poll is cheap, while tearing an SSE connection loses the
// connect-time snapshot (realtime.svelte.ts instead kicks its stream on the
// same visibilitychange event; the two mechanisms complement). Without a
// `document` (SSR, plain tests) this degrades to a bare interval.

/** Stops the interval and drops the visibility listener. Idempotent. */
export function startVisibleInterval(fn: () => void, ms: number): () => void {
  if (typeof document === "undefined") {
    const t = setInterval(fn, ms);
    return () => clearInterval(t);
  }
  const doc = document;
  let timer: ReturnType<typeof setInterval> | null = null;
  const arm = () => {
    if (timer === null) timer = setInterval(fn, ms);
  };
  const disarm = () => {
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
  const onVisibility = () => {
    if (doc.visibilityState === "visible") {
      arm();
      fn(); // foreground return wants fresh data now, not one period later
    } else {
      disarm();
    }
  };
  doc.addEventListener("visibilitychange", onVisibility);
  if (doc.visibilityState === "visible") arm();
  return () => {
    doc.removeEventListener("visibilitychange", onVisibility);
    disarm();
  };
}
