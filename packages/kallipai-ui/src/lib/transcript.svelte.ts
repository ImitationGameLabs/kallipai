// Headless auto-scroll controller for a scrolling transcript viewport. Encaps
// the "stick to tail while new lines arrive, unless the user scrolled up"
// behaviour the chat pages inline. DOM-bound but presentation-agnostic: the
// component binds the viewport element and calls `stick()` from an effect that
// reacts to content changes.

export interface AutoScrollOptions {
  /** Distance from the bottom (px) within which the viewport is "following". */
  readonly threshold?: number;
}

export interface AutoScroll {
  /** Bind the scroll container element to this. */
  viewport: HTMLDivElement | undefined;
  /** Whether we are currently pinned to the tail. */
  readonly follow: boolean;
  /** Lines that landed while not following (the jump-to-latest badge). */
  readonly missed: number;
  /** Attach to the viewport's `onscroll`. */
  onScroll: () => void;
  /**
   * Call from a content-change effect with the line count and the tail
   * line's stable key. Scrolls down only if following; while detached it
   * counts the appended lines into `missed` (a grown array whose tail key
   * did not move is an older-history prepend, not new conversation).
   */
  stick: (contentLength: number, tailKey?: string | number) => void;
  /** Unconditional jump to the tail: resume following, drop the count. */
  forceBottom: () => void;
  /** Re-anchor to the tail right now if following; a no-op while
   *  detached (user intent wins). The composer calls this after its
   *  height settles: the measure-collapse in resize() can clamp a
   *  bottom-pinned scrollTop against the transient layout, a net-zero
   *  resize the observer never sees. */
  reanchorIfFollowing: () => void;
  /** Return to fresh-mount state (follow, zero count, empty tail); the
   *  viewport binding survives. Call when the feeding session changes. */
  reset: () => void;
  /** Re-anchor to the tail on layout-driven viewport resizes (composer
   *  auto-grow, window resizes) while following. Returns its detacher. */
  observe: (target: HTMLElement) => () => void;
}

/** The tail snapshot one stick() call observes. */
export interface TailSnapshot {
  length: number;
  key: string | number | undefined;
}

/**
 * Lines to count as missed when content grows while not following: the
 * appended delta. A grown array whose tail key did not move is an
 * older-history prepend (the lazy-window pager), which is not new
 * conversation. Pure so the prepend fence is testable without a viewport
 * (the badgeLabel precedent in session/unread.svelte.ts).
 */
export function missedLines(
  prev: TailSnapshot,
  next: TailSnapshot,
  follow: boolean,
): number {
  if (follow) return 0;
  if (next.length <= prev.length) return 0;
  if (next.key === prev.key) return 0;
  return next.length - prev.length;
}

export function createAutoScroll(options: AutoScrollOptions = {}): AutoScroll {
  const threshold = options.threshold ?? 24;
  let viewport: HTMLDivElement | undefined = $state();
  let follow = $state(true);
  let missed = $state(0);
  let lastTail: TailSnapshot = { length: 0, key: undefined };

  function onScroll(): void {
    if (!viewport) return;
    const distanceFromBottom =
      viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight;
    follow = distanceFromBottom < threshold;
    // Back at the tail by hand: whatever piled up while away is seen now.
    if (follow) missed = 0;
  }

  function stick(contentLength: number, tailKey?: string | number): void {
    missed += missedLines(
      lastTail,
      { length: contentLength, key: tailKey },
      follow,
    );
    lastTail = { length: contentLength, key: tailKey };
    if (follow && viewport) viewport.scrollTop = viewport.scrollHeight;
  }

  function forceBottom(): void {
    follow = true;
    missed = 0;
    if (viewport) viewport.scrollTop = viewport.scrollHeight;
  }
  function reset(): void {
    follow = true;
    missed = 0;
    lastTail = { length: 0, key: undefined };
  }
  function reanchorIfFollowing(): void {
    if (viewport && follow) viewport.scrollTop = viewport.scrollHeight;
  }
  // Layout shifts (composer auto-grow, window resizes) shrink the scroll
  // container without firing a scroll event, so a following transcript
  // would visually drift off the tail and the next wheel tick would
  // then read as the user leaving. While following, re-anchor on any
  // container resize; while detached the observer is inert -- user
  // intent wins. Programmatic re-anchoring lands exactly on the tail,
  // so the follow-on scroll event re-affirms follow rather than
  // competing with it.
  function observe(target: HTMLElement): () => void {
    const ro = new ResizeObserver(reanchorIfFollowing);
    ro.observe(target);
    return () => ro.disconnect();
  }

  return {
    get viewport() {
      return viewport;
    },
    set viewport(value: HTMLDivElement | undefined) {
      viewport = value;
    },
    get follow() {
      return follow;
    },
    get missed() {
      return missed;
    },
    onScroll,
    stick,
    forceBottom,
    reanchorIfFollowing,
    reset,
    observe,
  };
}

/** Scroll-pin function for a raw-toggle on a single message bubble. The markdown/Shiki
 *  mount settles across several frames: when a taller-than-raw bubble collapses (or vice
 *  versa) the control the user just clicked would drift away from the cursor. `pin` keeps
 *  the clicked `anchor` (the actions row) at a fixed viewport-relative top while `target`
 *  (the bubble box) reflows.
 *
 *  ONE controller per transcript owns a single active ResizeObserver, so a rapid second
 *  toggle on a different bubble REPLACES the in-flight pin rather than stacking a second
 *  observer -- two observers each adjusting the shared `scrollTop` would over-correct and
 *  jitter. Pure (no DOM state of its own); the caller passes a `getViewport` that reads the
 *  live scroll container. */
export type TogglePin = (target: HTMLElement, anchor: HTMLElement) => void;

export function createTogglePin(
  getViewport: () => HTMLDivElement | undefined,
): TogglePin {
  let activeRO: ResizeObserver | undefined;
  return (target, anchor) => {
    const viewport = getViewport();
    if (!viewport) return;
    const topBefore = anchor.getBoundingClientRect().top;
    // Re-pin on every resize for a short window: the markdown/Shiki mount settles across
    // several frames, so a single rAF miss catches the rest.
    const doPin = (): void => {
      viewport.scrollTop += anchor.getBoundingClientRect().top - topBefore;
    };
    activeRO?.disconnect();
    requestAnimationFrame(doPin);
    const ro = new ResizeObserver(doPin);
    ro.observe(target);
    activeRO = ro;
    setTimeout(() => {
      ro.disconnect();
    }, 200);
  };
}
