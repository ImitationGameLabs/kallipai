// Shared class segments for the tonal icon-button family. The size token
// (size-7/8/10) and the contextual tail (e.g. shrink-0) stay inline at each
// call site so the markup keeps its per-context shape; these constants own
// only the segment every variant repeats. Extending the family means editing
// one constant instead of hunting the literal across pages and components.
export const TONAL_ICON_SURF =
  "grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500";

export const TONAL_ICON_PRIM =
  "grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-primary-500";

export const TONAL_ICON_ERR =
  "grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-error-500 hover:text-on-error-500";

// The per-message action button under a bubble (copy, raw toggle, quote): one
// faint icon that hover and focus reveal, and that stays visible on touch,
// where there is no hover to reveal it. One segment for all three, so the
// row cannot drift apart one button at a time.
// The hover reveal keys off an ancestor carrying `.group` (the row container
// holds it); a button does not bring its own.
export const BUBBLE_ACTION =
  "rounded p-1.5 text-surface-500 dark:text-surface-400 opacity-60 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover:opacity-100 focus-visible:opacity-100 hover:bg-surface-200-800 transition";
export const MENU_ITEM =
  "flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500";
export const MENU_ITEM_DANGER =
  "flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500";

// The ring a transcript bubble wears while a discard's landing put the focus
// on its row. A quote jump marks its row a moment, through `revealMark`, and
// is a separate face. :focus-visible
// answers false for a click and for a script focus alike (measured), so the
// marker is a one-shot `data-landing` attribute that `reveal.land` sets once
// the focus took and drops on blur: a click on a row leaves it unmarked, a
// landing that never took leaves none, and a hand-off a sighted reader made
// with the keyboard is visible. It sits on the bubble rather than the row,
// which spans the whole transcript column. The floor is the 3:1 of non-text
// indication; the mark aims to clear 4:1. Light: primary-800, 5.53:1 on the
// white page, 4.12:1 on the own-message fill and 4.59:1 on the agent one;
// one step darker clears more (primary-900 reaches 7.63 / 5.69 / 6.34) but
// reads as a hard edge over the pale bubble, louder than the message. Dark:
// primary-200, 7.72:1 on the page and 4.83:1 on the own-message fill; the
// primary-500 it replaced sat 0.18 above the 3:1 floor, too thin for a mark.
export const ROW_LANDED =
  "rounded-base data-[landing]:outline-2 data-[landing]:outline-offset-2 data-[landing]:outline-primary-800 dark:data-[landing]:outline-primary-200";

// A discussion's entry bubble: the room list is one row per discussion, so the
// row that holds one has to say so before the reader hovers it. The ring is
// that mark, and hover thickens it rather than recolouring it -- the mark IS
// the ring, and a hover that swapped its colour would erase the reading. The
// polarity pair is ROW_LANDED's, measured again on the entry's own fills:
// primary-800 sits 4.59:1 on the agent bubble and 4.12:1 on the own-message
// fill; dark primary-200 sits 6.42:1 and 4.83:1 on the same two. ROW_LANDED's
// ledger states the light pairs and the dark own-fill one, so the dark figure
// for the agent bubble is measured here rather than quoted. The pointer and the
// shadow transition ride along, so the ring arrives without a jolt.
export const BUBBLE_ENTRY_MARK =
  "cursor-pointer transition-shadow ring-1 ring-primary-800 dark:ring-primary-200 hover:ring-2";

// A discussion entry's preview row: the reply body leads and its author trails
// at the row's own size, dropped to 0.8 opacity. At 0.6 a name that size
// measures 3.02:1 on the card in light and 3.48:1 in dark, under what text
// owes; 0.8 measures 4.77 and 4.90 and still reads quieter than the reply it
// summarizes. The gap is 16px: at 6px a body that filled its box ended a
// word-space short of the handle, so the name read as the last word of the
// sentence. A handle longer than the row can hold is capped at half of it and
// cut with an ellipsis: at 1280px a 32-character handle outgrew the card
// rather than the body giving way. The full name stays in the title, and the
// discussion's own page writes it out beside every reply.
export const PREVIEW_ROW = "flex items-baseline gap-4 min-w-0";
export const PREVIEW_HANDLE =
  "ml-auto shrink-0 opacity-80 max-w-[50%] truncate";

// The chat transcript's column: one centred measure every chat surface draws its
// rows inside, so the direct, room, channel and discussion pages cannot drift
// into different widths and paddings. The scroll pill hangs in the same measure.
export const TRANSCRIPT_COLUMN =
  "mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3";

// That pill. The raised form is the one that may outrank a later sibling of the
// hosting page; the room page keeps the bare form on purpose, because its
// members panel is a later sibling at the same level and has to win.
export const SCROLL_PILL =
  "absolute inset-x-0 bottom-4 mx-auto flex max-w-[80rem] justify-end px-4 pointer-events-none";
export const SCROLL_PILL_RAISED = `${SCROLL_PILL} z-10`;
