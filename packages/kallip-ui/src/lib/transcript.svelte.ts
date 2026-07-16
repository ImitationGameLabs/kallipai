// Headless auto-scroll controller for a scrolling transcript viewport. Encaps
// the "stick to tail while new lines arrive, unless the user scrolled up"
// behaviour that TranscriptView previously inlined. DOM-bound but
// presentation-agnostic: the component binds the viewport element and calls
// `stick()` from an effect that reacts to content changes.

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
