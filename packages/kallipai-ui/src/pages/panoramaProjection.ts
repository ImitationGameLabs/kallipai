// The panorama sessions region's projection: merge the conversation sources
// into one list, pin unread rows (badge > 0) above read rows with each group
// keeping its source order, and cap the visible rows -- /chats is the
// unbounded view. Pure data in, pure data out: the page maps its stores into
// this shape and renders the result, so the ordering rule stays unit-testable
// without stores.

export interface PanoramaSessionRow {
  href: string;
  label: string;
  kind: "tagma" | "room" | "direct";
  badge?: number;
}

export function panoramaSessionRows(
  rows: PanoramaSessionRow[],
  cap: number,
): PanoramaSessionRow[] {
  const unread = rows.filter((r) => (r.badge ?? 0) > 0);
  const read = rows.filter((r) => (r.badge ?? 0) === 0);
  return [...unread, ...read].slice(0, Math.max(cap, 0));
}
