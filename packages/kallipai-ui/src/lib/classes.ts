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
