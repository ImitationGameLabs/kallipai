// The per-row interaction both chat transcripts apply: which row's overflow
// menu is open, and the touch hold that opens the same menu a pointer device
// reveals on hover. One open id and one hold timer live here; each surface
// still binds the same four pointer events, and only two domain facts differ
// between the surfaces: how a row is numbered, and what makes it quotable.
// Those arrive as options; everything else lives here.
//
// One menu per transcript is the rule, so the open id cannot live in each row:
// opening another row's menu closes the first. The hold is here for the same
// reason -- one timer per transcript -- and because a timer outliving the view
// would open a menu on a torn-down one.
//
// The open entry carries the conversation it was opened in (the host's
// `scope` option), and `isMenuOpen` compares that with the live scope, so
// a row id from one conversation matches nothing in the next (a room
// numbers its messages from 1). Ownership travels with the state instead
// of the caller: a host that never resets still shows no menu in the wrong
// conversation. The hold timer records the conversation it was armed in
// too, which covers what the entry cannot -- a hold coming due after the
// reader left stays shut.
//
// Revealing -- the row table a quote jumps to -- stays in reveal.svelte.ts: it
// is a different mechanism, already shared.

/** How long a touch hold takes to open the row's menu, in milliseconds. */
const HOLD_MS = 500;

/** The one field of a pointer event a hold reads. A real `PointerEvent`
 *  satisfies it; naming the field keeps this module free of the DOM types the
 *  test harness does not carry. */
export interface PointerPress {
  readonly pointerType: string;
}

export interface RowActions<R> {
  /** Whether this row's menu is the open one (the `MessageActions` prop). */
  isMenuOpen(row: R): boolean;
  /** The row's menu asked to open or close (the `MessageActions` handler). A
   *  close from another row is ignored: a stale close must not clear a menu the
   *  user just opened elsewhere; a close for the same id from another
   *  conversation is ignored the same way. */
  setMenuOpen(row: R, open: boolean): void;
  /** Pointer down: a touch hold opens the menu. A mouse press is not a hold
   *  (that is what hover is for), and a row with no affordance is ignored, so a
   *  hold can never open an empty menu. */
  pressStart(row: R, event: PointerPress): void;
  /** Pointer up, cancel or leave: the hold is off. */
  pressCancel(): void;
  /** The row's quote entry point. */
  quote(row: R): void;
  /** Drop the open menu and a hold in flight now: the host calls this when its
   *  session changes, beside the other per-session resets, and again at
   *  teardown. The entry's own scope only hides it -- returning to the
   *  conversation it was opened in would show it again -- so this is what
   *  keeps a return trip from resurrecting a menu the reader left. */
  reset(): void;
}

export interface RowActionsOptions<R> {
  /** The conversation these rows belong to: a different value stops matching
   *  the open id and disarms a hold in flight. A getter, so the check reads the
   *  live route. Undefined = this transcript has no session identity (a static
   *  caller); the entry then lives until the host resets. */
  readonly scope: () => string | undefined;
  /** How this surface numbers a row: a history id, or a room seq. */
  readonly idOf: (row: R) => number;
  /** Whether the row can be quoted at all: an optimistic or failed row has no
   *  id the other side could resolve. */
  readonly quotable: (row: R) => boolean;
  /** What a quote request does (the page arms its pending quote). The row
   *  renders the entry whenever it is quotable; absent, `quote` is a no-op
   *  and the host alone decides whether to offer one. */
  readonly onQuote?: (row: R) => void;
  /** Test seam: both surfaces keep the default 500ms; a test passes a shorter
   *  hold. */
  readonly holdMs?: number;
}

export function createRowActions<R>(
  options: RowActionsOptions<R>,
): RowActions<R> {
  const holdMs = options.holdMs ?? HOLD_MS;
  /** The row whose menu is open, and the conversation it was opened in.
   *  Reactive: the row's menu draws from this during render. */
  let menuOpen = $state<{ id: number; scope: string | undefined } | null>(null);
  let hold: ReturnType<typeof setTimeout> | undefined;

  return {
    isMenuOpen(row) {
      return (
        menuOpen !== null &&
        menuOpen.scope === options.scope() &&
        menuOpen.id === options.idOf(row)
      );
    },
    setMenuOpen(row, open) {
      const id = options.idOf(row);
      if (open) menuOpen = { id, scope: options.scope() };
      else if (
        menuOpen !== null &&
        menuOpen.id === id &&
        menuOpen.scope === options.scope()
      ) {
        menuOpen = null;
      }
    },
    pressStart(row, event) {
      if (event.pointerType === "mouse" || !options.quotable(row)) return;
      clearTimeout(hold);
      const armedIn = options.scope();
      const id = options.idOf(row);
      hold = setTimeout(() => {
        if (options.scope() === armedIn) menuOpen = { id, scope: armedIn };
      }, holdMs);
    },
    pressCancel() {
      clearTimeout(hold);
    },
    quote(row) {
      options.onQuote?.(row);
    },
    reset() {
      clearTimeout(hold);
      menuOpen = null;
    },
  };
}
