// Scrolling a reader to a row that is already on screen, moving the focus onto
// it, saying which row a quote pointed at, and marking it for a moment.
// Extracted from the 1:1 transcript so the room transcript can jump too: the
// two surfaces key their rows differently (a history id vs a room seq) and
// register different markup, but the mechanism is one -- a row table,
// `scrollIntoView`, one focus move, one sentence, one highlight.
//
// The table is a plain Map, not reactive state: it is written from a Svelte
// action and read from event handlers only, never from a render.
//
// Rows are registered by the `trackRow` action, which the caller attaches with
// `use:reveal.trackRow={id}`; a keyed each block re-runs it, so a node maps to
// one id for its lifetime.

// Whether a row can be jumped to, and whether it can be named, are decided
// before anything moves: no element or no sentence, no jump -- nothing
// scrolls, no focus moves, no announcement. The injected `describe` answers
// only what a reached row is called, so a surface cannot offer a jump it
// cannot announce, nor announce one it never made.

/** How long a revealed row stays marked, in milliseconds. */
const REVEAL_FLASH_MS = 1600;

interface Reveal {
  /** The row id currently marked, or null. */
  readonly highlightId: number | null;
  /** What the last jump said out loud, or undefined. Scoped like the mark: a
   *  sentence about the row the previous session showed says nothing about
   *  this one. */
  readonly announcement: string | undefined;
  /** Which of the two spoken slots currently carries the sentence, 0 or 1.
   *  A live region speaks only when its content moves, and two rows can be
   *  called the same thing: each jump writes into the other slot, so an equal
   *  sentence is still a change in the region that receives it. */
  readonly slot: 0 | 1;
  /** Register a row element under `id`. Svelte action: returns its cleanup. */
  trackRow(node: HTMLElement, id: number): { destroy(): void };
  /** Scroll a registered, nameable row into view, move the focus onto it, and
   *  say what it is. False means the jump did not happen -- the row is not
   *  registered, or nothing can name it: nothing moves and nothing is said. */
  jump(id: number): boolean;
  /** Clear the flash timer, the mark and the announcement. Call from the
   *  owner's teardown, and from the cleanup of the effect that owns the
   *  scope: a timer outliving the transcript would touch torn-down state.
   *  A cleanup is the safe place for it -- no reaction is registered there,
   *  so the spoken slot it reads back to flip is never a dependency. */
  destroy(): void;
}

export function createReveal(options: {
  // Test seam: production callers omit this and keep REVEAL_FLASH_MS.
  readonly flashMs?: number;
  /** The session the mark and the announcement belong to. A row id is only a
   *  number, and the next session numbers its own rows from its own scale: a
   *  flash or a sentence that outlived the swap would describe a stranger
   *  row. Compared by value on every read, so both are scoped without the
   *  host having to reset them (the row table already drops itself with the
   *  rows). */
  readonly scope?: () => string | undefined;
  /** What a reached row is called out loud, or undefined when the row cannot
   *  be named at all. The wording stays with the caller: one surface speaks a
   *  roster name and a time, the other a handle and a send time. */
  readonly describe: (id: number) => string | undefined;
}): Reveal {
  const flashMs = options.flashMs ?? REVEAL_FLASH_MS;
  let highlightId = $state<number | null>(null);
  let announcement = $state<string | undefined>(undefined);
  let slot = $state<0 | 1>(0);
  /** The scope the mark and the announcement were made in; undefined when
   *  the caller scoped none. */
  let markedScope: string | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const rowEls = new Map<number, HTMLElement>();

  /** Put a sentence in the other spoken slot, so the region that receives it
   *  sees a change even when the words are the ones it heard last time. */
  function say(said: string): void {
    announcement = said;
    slot = slot === 0 ? 1 : 0;
  }

  /** Empty the spoken slots and flip them, for the same reason. */
  function silence(): void {
    announcement = undefined;
    slot = slot === 0 ? 1 : 0;
  }

  function flash(id: number): void {
    highlightId = id;
    markedScope = options.scope?.();
    clearTimeout(timer);
    timer = setTimeout(() => {
      highlightId = null;
      // The sentence belongs to the moment of the jump: it goes with the
      // mark, so a session left and re-entered cannot replay it.
      silence();
    }, flashMs);
  }

  function trackRow(node: HTMLElement, id: number) {
    rowEls.set(id, node);
    return {
      destroy() {
        rowEls.delete(id);
      },
    };
  }

  function jump(id: number): boolean {
    const el = rowEls.get(id);
    if (!el) return false;
    // What the row is called is asked first: an unnameable row is not jumped
    // to at all, rather than scrolled to in silence.
    const said = options.describe(id);
    if (said === undefined) return false;
    el.scrollIntoView({ block: "center", inline: "nearest" });
    // The scroll is placed; the focus move must not drag it back. The row is
    // a focus target only so this move has somewhere to land.
    el.focus({ preventScroll: true });
    say(said);
    flash(id);
    return true;
  }

  function destroy(): void {
    clearTimeout(timer);
    highlightId = null;
    silence();
  }

  // A mark or a sentence is only about the rows of the session it was made
  // in: read both through the scope, so a swap drops them without a reset.
  function inScope(): boolean {
    const scope = options.scope;
    return scope === undefined || scope() === markedScope;
  }

  return {
    get highlightId() {
      return inScope() ? highlightId : null;
    },
    get announcement() {
      return inScope() ? announcement : undefined;
    },
    get slot() {
      return slot;
    },
    trackRow,
    jump,
    destroy,
  };
}

/** The mark a revealed row wears while its flash is on, as the class string
 * the row appends to its own. One line, keyed by the controller's highlight
 * id, so both transcripts mark the same way and a key drift is one edit. */
export function revealMark(highlightId: number | null, id: number): string {
  return highlightId === id ? "rounded-base ring-2 ring-primary-500" : "";
}
