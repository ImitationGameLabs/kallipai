// Docs content pipeline — the single module behind the /docs/ section.
// Loads the repo docs/ markdown trees at build time (import.meta.glob
// with ?raw + eager inlines file contents into the bundle): docs/en/ is
// the en canon; docs/zh-cn/ is the zh-cn tree, built the same way.
// Frontmatter is zod-validated; ordering, neighbor lookup, and the
// comark parse step live here. comark is 0.x: calls stay in this file.
import { createMarkdownParser, parseFrontmatter } from "comark";
import toc from "comark/plugins/toc";
import {
  internalLinkMessage,
  normalizeSlug,
  resolveTocAnchor,
  segmentRelativeHref,
  stripDocPrefix,
  ungroupedSlugs,
} from "./doc-links.ts";
import { docGroups, flattenGroups, groupOrder } from "./doc-tree.ts";
import { frontmatterSchema } from "./doc-entry.ts";
import type { DocEntry } from "./doc-entry.ts";

export type { DocEntry, Frontmatter } from "./doc-entry.ts";

const EN_ROOT = "../../../../docs/en/";
const ZH_ROOT = "../../../../docs/zh-cn/";
const rawFiles = import.meta.glob("../../../../docs/en/**/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

const parsed = Object.entries(rawFiles)
  .map(([key, source]) => {
    const { content, data } = parseFrontmatter(source);
    return {
      slug: stripDocPrefix(key, EN_ROOT),
      frontmatter: frontmatterSchema.parse(data),
      body: content,
      source,
    } satisfies DocEntry;
  })
  .filter((doc) => !doc.frontmatter.internal);

// zh-cn is its own markdown tree; the zh view lists exactly the slugs
// that exist here — untranslated en pages never fall through.
const zhParsed = Object.entries(
  import.meta.glob("../../../../docs/zh-cn/**/*.md", {
    query: "?raw",
    import: "default",
    eager: true,
  }) as Record<string, string>,
).map(([key, source]) => {
  const { content, data } = parseFrontmatter(source);
  return {
    slug: stripDocPrefix(key, ZH_ROOT),
    frontmatter: frontmatterSchema.parse(data),
    body: content,
    source,
  } satisfies DocEntry;
});

// The en canon: the en segment and the locale-independent surfaces (sitemap,
// llms, the /md mirror) all read this list. The flat order is the nav
// tree's preorder traversal — the tree in doc-tree.ts is the single
// ordering source, and nothing downstream may re-sort.
export const docs: DocEntry[] = flattenGroups(docGroups(parsed));

// The zh-cn view: only the translations that exist — no en fallback.
// Untranslated slugs are absent from the zh tree, nav, and sitemap.
export const zhDocs: DocEntry[] = flattenGroups(docGroups(zhParsed));

export type { DocGroupNode, DocNavNode } from "./doc-tree.ts";
export {
  docGroups,
  navBranchIds,
  navIdByHref,
  toNavNodes,
} from "./doc-tree.ts";

// A slug outside every nav group still renders as a detail page but
// lists in no sidebar or llms group: the build says so at load time.
const ungrouped = ungroupedSlugs(
  docs.map((doc) => doc.slug),
  groupOrder,
);
if (ungrouped.length > 0) {
  console.warn(`[docs] slug has no nav group: ${ungrouped.join()}`);
}

const zhUngrouped = ungroupedSlugs(
  zhDocs.map((doc) => doc.slug),
  groupOrder,
);
if (zhUngrouped.length > 0) {
  console.warn(`[docs] zh-cn slug has no nav group: ${zhUngrouped.join()}`);
}

// The en canon: the en segment and the locale-independent surfaces (sitemap,
// llms, the /md mirror) all read this list.
export function findDoc(
  slug: string,
  list: readonly DocEntry[] = docs,
): DocEntry | undefined {
  return list.find((doc) => doc.slug === slug);
}

// Neighbors over the same sorted sequence the sidebar renders, so the pager
// and the sidebar can never disagree.
export function neighbors(
  slug: string,
  list: readonly DocEntry[] = docs,
): {
  prev: DocEntry | undefined;
  next: DocEntry | undefined;
} {
  const index = list.findIndex((doc) => doc.slug === slug);
  return {
    prev: index > 0 ? list[index - 1] : undefined,
    next: index >= 0 && index < list.length - 1 ? list[index + 1] : undefined,
  };
}

// One parser instance (TOC plugin on) for every page; the TOC tree arrives in
// document.meta.toc and the same document drives rendering.
const parse = createMarkdownParser({ plugins: [toc({ depth: 3 })] });

export type ParsedDoc = Awaited<ReturnType<typeof parse>>;

// Docs sources keep repo-relative links — GitHub stays canonical for raw
// reading. At parse time, relative .md targets ("./x.md", "../y/x.md")
// resolve against the current doc's directory and rewrite to segment-
// relative site routes (../../<slug>/ from a nested page), which stay
// inside whichever locale tree renders them. Cross-page anchors arrive
// in GitHub slug form, but comark ids are parent-qualified, so each
// anchor is resolved against the target document's TOC (which carries
// the real ids); absolute URLs and pure anchors pass through untouched.
function rewriteHref(
  href: string,
  // The doc file's own directory (repo-relative resolution基准).
  fromDir: string,
  // The page URL's directory (the crawl-out depth for rewritten links).
  urlFromDir: string,
  resolveAnchor: (slug: string, anchor: string) => string,
): string {
  const hash = href.indexOf("#");
  const anchor = hash === -1 ? "" : href.slice(hash + 1);
  const path = hash === -1 ? href : href.slice(0, hash);
  // Absolute URLs, site-root paths, and pure anchors are not repo-relative.
  if (/^([a-z][a-z0-9+.-]*:|\/|#)/i.test(href)) return href;
  if (!path.endsWith(".md")) return href;
  const stack: string[] = [];
  for (const segment of ((fromDir ? fromDir + "/" : "") + path).split("/")) {
    if (segment === "..") stack.pop();
    else if (segment !== "." && segment !== "") stack.push(segment);
  }
  const slug = normalizeSlug(stack.join("/").replace(/\.md$/, ""));
  // Unresolvable targets (missing doc, or ../ climbing out of docs/) keep
  // the original repo href; prerender fail-fast is the backstop.
  if (!findDoc(slug)) {
    // Internal target: the warning is the leading diagnostic; the
    // prerender 404 on the kept repo path is the enforcement.
    const internal = internalLinkMessage(
      slug,
      rawFiles[EN_ROOT + slug + ".md"] !== undefined,
    );
    if (internal) console.warn(internal);
    return href;
  }
  return segmentRelativeHref(
    slug,
    urlFromDir,
    anchor ? resolveAnchor(slug, anchor) : "",
  );
}

function rewriteLinks(
  nodes: unknown[],
  fromDir: string,
  urlFromDir: string,
  ownSlug: string,
  resolveAnchor: (slug: string, anchor: string) => string,
): void {
  for (const node of nodes) {
    if (!Array.isArray(node)) continue;
    const [tag, attrs, ...children] = node as [
      string,
      Record<string, unknown>,
      ...unknown[],
    ];
    if (tag === "a" && attrs && typeof attrs.href === "string") {
      // Same-page anchors are GitHub-slug form too; resolve them against
      // this document's own TOC ids (comark ids are parent-qualified).
      if (attrs.href.startsWith("#")) {
        attrs.href = "#" + resolveAnchor(ownSlug, attrs.href.slice(1));
      } else {
        attrs.href = rewriteHref(
          attrs.href,
          fromDir,
          urlFromDir,
          resolveAnchor,
        );
      }
    }
    rewriteLinks(children, fromDir, urlFromDir, ownSlug, resolveAnchor);
  }
}

// Parsed document caches, one per view: each view parses its own entries,
// so the zh-cn view renders its own body, its own TOC, and its own
// rewritten links while the en view stays untouched. Everything is parsed
// up front (prerender touches every page anyway) so cross-page anchors
// resolve against the target's own view TOC in a second pass.
// The list arguments must stay the exported docs/zhDocs singletons:
// caches is keyed by array identity, so a fresh array misses and re-parses.
const caches = new Map<readonly DocEntry[], Map<string, ParsedDoc>>();
const viewParsing = new Map<readonly DocEntry[], Promise<void>>();

export function parseDoc(
  entry: DocEntry,
  list: readonly DocEntry[] = docs,
): Promise<ParsedDoc> {
  let parsing = viewParsing.get(list);
  parsing ??= (async () => {
    const parsedDocs = new Map<string, ParsedDoc>();
    caches.set(list, parsedDocs);
    await Promise.all(
      list.map(async (doc) => parsedDocs.set(doc.slug, await parse(doc.body))),
    );
    for (const doc of list) {
      const document = parsedDocs.get(doc.slug);
      if (!document) continue;
      // Repo-relative targets resolve against the doc file's own
      // directory (an index page sits in the directory its slug names);
      // rewritten links crawl out of the page URL's directory, which is
      // the slug minus its last segment for regular and index pages
      // alike (.../deployment/nixos/ crawls the same two levels as
      // .../deployment/nixos/minimal/).
      const cut = doc.slug.lastIndexOf("/");
      const urlFromDir = cut === -1 ? "" : doc.slug.slice(0, cut);
      const fileDir = doc.indexPage ? doc.slug : urlFromDir;
      rewriteLinks(
        document.nodes as unknown[],
        fileDir,
        urlFromDir,
        doc.slug,
        (slug, anchor) => resolveTocAnchor(parsedDocs.get(slug), anchor),
      );
    }
  })();
  viewParsing.set(list, parsing);
  return parsing.then(() => {
    const parsedDocs = caches.get(list);
    const document = parsedDocs?.get(entry.slug);
    if (!document) throw new Error(`doc not parsed: ${entry.slug}`);
    return document;
  });
}
