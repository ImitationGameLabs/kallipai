// The four profile limit fields as typed: blank is no limit, a number rides
// as itself, and text no number can be read from keeps the save disabled.
// Sent as a number, NaN or Infinity would serialize to null, the wire's
// "no limit", so the refusal has to happen here.

/** The field for the wire: null is blank (no limit), a number is itself,
 * undefined is text no number can be read from -- there is no number to send,
 * so the save stays disabled. */
export function parseProfileLimit(raw: string): number | null | undefined {
  const text = raw.trim();
  if (text === "") return null;
  const n = Number(text);
  return Number.isFinite(n) ? n : undefined;
}
