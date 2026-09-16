// Sitemap URL-set builder: pure string work so the route handler stays a
// thin wrapper and the coverage rules are unit-testable. Locale hrefs come
// from lang.ts enHref/zhHref — the same single prefix source as the
// language switch, so the sitemap's hreflang can never disagree with it.
import { enHref, zhHref } from "./lang.ts";

// The English route list is the site's human-facing page set: bare paths
// plus one entry per docs page. zh-cn entries are derived, never hand-listed.
// Raw-markdown mirrors and the llms.txt endpoints are deliberately absent:
// a sitemap indexes crawlable HTML pages, not their transport formats.
export function enPagePaths(docSlugs: string[]): string[] {
  return [
    "/",
    "/about/",
    "/terms/",
    "/privacy/",
    "/docs/",
    ...docSlugs.map((slug) => `/docs/${slug}/`),
  ];
}

// Both locale blocks of one path (en loc + zh loc) come from here - the
// en/zh URLs derive inside, so the caller cannot pass a mismatched pair.
// Each block carries the full alternate set (en, zh-cn, x-default) as the
// sitemap protocol documents for language variants; x-default points at
// the default-language (en) version, the common convention. loc and href
// values are XML-escaped: slugs come from the docs tree, and a future
// slug carrying & or < must not break the document.
function escapeXml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function urlBlock(siteUrl: string, path: string): string {
  const en = escapeXml(siteUrl + enHref(path));
  const zh = escapeXml(siteUrl + zhHref(path));
  const link = (hreflang: string, href: string) =>
    `    <xhtml:link rel="alternate" hreflang="${hreflang}" href="${href}"/>`;
  const block = (loc: string) =>
    [
      "  <url>",
      `    <loc>${loc}</loc>`,
      link("en", en),
      link("zh-cn", zh),
      link("x-default", en),
      "  </url>",
    ].join("\n");
  return [block(en), block(zh)].join("\n");
}

export function sitemapXml(siteUrl: string, docSlugs: string[]): string {
  const blocks = enPagePaths(docSlugs).map((path) => urlBlock(siteUrl, path));
  return (
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
    `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"\n` +
    `        xmlns:xhtml="http://www.w3.org/1999/xhtml">\n` +
    blocks.join("\n") +
    `\n</urlset>\n`
  );
}
