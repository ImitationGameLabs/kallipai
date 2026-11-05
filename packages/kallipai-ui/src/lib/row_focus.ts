// Where focus goes after a transcript row leaves. A confirmed discard takes the
// row and the trigger that owned it out together, so the browser drops focus on
// <body> and a reader has to Tab from the top of the document again.
//
// The rule is reading position, not proximity: the row above the one that left,
// the row below when it was the first, and nothing when the transcript is
// empty -- the caller then hands focus to the composer.
//
// What comes in is the order that REMAINS, plus the place the departed row
// held in the order that was rendered. Reading the order after the store's
// write is the point: the store drops the line as it answers, so a list
// captured before the write is the only one that still names the row, and a
// list captured after it is the only one whose answer is a row still there.
// Both cannot be satisfied by an id lookup, which is why the position, not the
// id, is what the caller passes.

/** The row to land on after the row at `index` left `idsAfter`: the entry
 *  before that place, else the entry that took it, else null when nothing is
 *  left. An index outside the remaining order means the caller is describing a
 *  transcript this one is not, and gets null rather than the first row: a
 *  wrong answer here moves a reader to the top of the transcript. */
export function neighbourRow(
  idsAfter: readonly number[],
  index: number,
): number | null {
  if (index < 0 || index > idsAfter.length) return null;
  if (index > 0) return idsAfter[index - 1]!;
  return idsAfter[0] ?? null;
}
