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
