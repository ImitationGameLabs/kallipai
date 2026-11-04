// Where the focus lands when a turn row gets an arrow key.
//
// Split out of the list so the rule is testable without a browser: the list
// hands in the row it is on and how many rows it has, and gets back the row to
// focus, or null for a key that is not about walking the list.
//
// The ends do not wrap. The list pages older turns with a "load more" button
// under the last row, so an arrow that jumped from the end to the start would
// carry the reader past the turns they were walking towards.

/** The row to focus for `key`, or null when the key is not a walk. */
export function stepIndex(
  index: number,
  count: number,
  key: string,
): number | null {
  if (count <= 0) return null;
  // -1 means the walk starts at the top: either arrow lands on the row it
  // would step to from there. An index past either end is clamped to that
  // end first, so every answer names a row that exists.
  const from = index < 0 ? -1 : Math.min(index, count - 1);
  switch (key) {
    case "ArrowDown":
      return Math.min(from + 1, count - 1);
    case "ArrowUp":
      return Math.max(from - 1, 0);
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return null;
  }
}
