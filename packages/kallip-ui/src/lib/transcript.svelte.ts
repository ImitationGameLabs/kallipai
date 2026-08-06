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
  /** Attach to the viewport's `onscroll`. */
  onScroll: () => void;
  /** Call from a content-change effect; scrolls down only if following. */
  stick: () => void;
}

export function createAutoScroll(options: AutoScrollOptions = {}): AutoScroll {
  const threshold = options.threshold ?? 24;
  let viewport: HTMLDivElement | undefined = $state();
  let follow = $state(true);

  function onScroll(): void {
    if (!viewport) return;
    const distanceFromBottom =
      viewport.scrollHeight - viewport.scrollTop - viewport.clientHeight;
    follow = distanceFromBottom < threshold;
  }

  function stick(): void {
    if (follow && viewport) viewport.scrollTop = viewport.scrollHeight;
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
    onScroll,
    stick,
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
