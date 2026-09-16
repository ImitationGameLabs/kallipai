// Segment-relative doc links: from a rendered doc page, climb out of the
// doc's own directory plus one more level for the docs root itself, then
// step into the target slug. Identical shape under both locale trees.
import type { TocLink } from "comark/plugins/toc";
export function segmentRelativeHref(
  slug: string,
  fromDir: string,
  anchor: string,
): string {
  const depth = (fromDir ? fromDir.split("/").length : 0) + 1;
  return "../".repeat(depth) + `${slug}/${anchor ? "#" + anchor : ""}`;
}

// GitHub-slug anchors vs parent-qualified comark ids: compare on a
// normalized form (GitHub keeps punctuation runs as multiple dashes, comark
// collapses them); fall back through a suffix match, then pass the anchor
// through untouched so the page keeps a stable in-page target.
export function resolveTocAnchor(
  target: { meta: { toc?: { links?: TocLink[] } } } | undefined,
  anchor: string,
): string {
  const links = target?.meta.toc?.links;
  if (!links) return anchor;
  // nestHeaders nests h3+ under their parent, so flatten before matching.
  const ids: string[] = [];
  const walk = (links: TocLink[]) => {
    for (const link of links) {
      ids.push(link.id);
      if (link.children) walk(link.children);
    }
  };
  walk(links);
  const norm = (s: string) => s.replace(/-+/g, "-");
  return (
    ids.find((id) => norm(id) === norm(anchor)) ??
    ids.find((id) => norm(id).endsWith("-" + norm(anchor))) ??
    anchor
  );
}
// Message for a .md link target that resolves inside docs/en/ but is
// missing from the published list: that is an internal doc. undefined
// for a plain missing doc (the kept repo path says all it needs to).
export function internalLinkMessage(
  slug: string,
  inTree: boolean,
): string | undefined {
  return inTree
    ? `[docs] internal doc linked from a published page: ${slug}`
    : undefined;
}

// Slugs whose top segment is outside the nav groups (root docs rank as
// docs, mirroring groupRank): they render but no group lists them.
export function ungroupedSlugs(
  slugs: readonly string[],
  groups: readonly string[],
): string[] {
  return slugs.filter((slug) => {
    const slash = slug.indexOf("/");
    const top = slash === -1 ? "docs" : slug.slice(0, slash);
    return !groups.includes(top);
  });
}
