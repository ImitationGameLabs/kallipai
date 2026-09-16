// Segment-relative doc links: from a rendered doc page, climb out of the
// doc's own directory plus one more level for the docs root itself, then
// step into the target slug. Identical shape under both locale trees.
export function segmentRelativeHref(
  slug: string,
  fromDir: string,
  anchor: string,
): string {
  const depth = (fromDir ? fromDir.split("/").length : 0) + 1;
  return "../".repeat(depth) + `${slug}/${anchor ? "#" + anchor : ""}`;
}
