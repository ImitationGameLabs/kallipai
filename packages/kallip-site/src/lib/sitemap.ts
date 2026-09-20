// Sitemap URL-set builder: pure string work so the route handler stays a
// thin wrapper and the coverage rules are unit-testable. Locale hrefs come
// from lang.ts enHref/zhHref — the same single prefix source as the
// language switch, so the sitemap's hreflang can never disagree with it.
import { enHref, zhHref } from "./lang.ts";

// The English route list is the site's human-facing page set: bare paths
// plus one entry per docs page. zh-cn entries are derived, never hand-listed.
// Raw-markdown mirrors and the llms.txt endpoints are deliberately absent:
// a sitemap indexes crawlable HTML pages, not their transport formats.
// Static pages live under both locale trees; docs pages pair with a
// zh mirror only when the zh tree carries the translation.
const STATIC_PATHS = ["/", "/about/", "/terms/", "/privacy/"];

export function enPagePaths(docSlugs: string[]): string[] {
  return [...STATIC_PATHS, ...docSlugs.map((slug) => `/docs/${slug}/`)];
}

// One path's url face: the en block always; the zh block and its
// zh-cn alternate only when the zh tree carries the path (hasZh).
// URLs derive inside, so the caller cannot pass a mismatched pair.
// Each block carries the sitemap-protocol alternate set (en, zh-cn,
// x-default); x-default points at the default-language (en) version.
// values are XML-escaped: slugs come from the docs tree, and a future
// slug carrying & or < must not break the document.
function escapeXml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function urlBlock(siteUrl: string, path: string, hasZh: boolean): string {
  const en = escapeXml(siteUrl + enHref(path));
  const zh = escapeXml(siteUrl + zhHref(path));
  const link = (hreflang: string, href: string) =>
    `    <xhtml:link rel="alternate" hreflang="${hreflang}" href="${href}"/>`;
  const block = (loc: string) =>
    [
      "  <url>",
      `    <loc>${loc}</loc>`,
      link("en", en),
      ...(hasZh ? [link("zh-cn", zh)] : []),
      link("x-default", en),
      "  </url>",
    ].join("\n");
  return (hasZh ? [block(en), block(zh)] : [block(en)]).join("\n");
}

export function sitemapXml(
  siteUrl: string,
  docSlugs: string[],
  zhDocSlugs: string[],
): string {
  // Docs slugs pair with their zh mirror only when translated; the
  // static pages exist under both locale trees unconditionally.
  const zhPaths = new Set([
    ...STATIC_PATHS,
    ...zhDocSlugs.map((slug) => `/docs/${slug}/`),
  ]);
  const blocks = enPagePaths(docSlugs).map((path) =>
    urlBlock(siteUrl, path, zhPaths.has(path)),
  );
  return (
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"\n` +
    `        xmlns:xhtml="http://www.w3.org/1999/xhtml">\n` +
    blocks.join("\n") +
    `\n</urlset>\n`
  );
}
