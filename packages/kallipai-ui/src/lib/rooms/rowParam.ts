// A row number read out of a URL: a discussion's root sits in the path, and
// `?at=` / `?quote=` name a row inside it. Rows are numbered from 1 upwards by
// whole numbers, so anything else is a stale or hand-edited link rather than a
// row to read: the pages send such a link back to the room instead of opening
// an empty discussion.

/** Rows start at 1 and carry no sign, padding or fraction. */
const ROW = /^[1-9][0-9]*$/;

/** The row a URL value names, or null when it names none. */
export function rowParamOf(value: string | undefined): number | null {
  if (value === undefined || !ROW.test(value)) return null;
  const row = Number(value);
  return Number.isSafeInteger(row) ? row : null;
}
