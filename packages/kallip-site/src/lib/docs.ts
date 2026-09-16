// Docs content pipeline — the single module behind the /docs/ section.
// Loads the repo docs/ markdown trees at build time (import.meta.glob
// with ?raw + eager inlines file contents into the bundle): docs/en/ is
// the en canon, docs/zh-cn/ carries per-slug zh overrides on top of it.
// Frontmatter is zod-validated; ordering, neighbor lookup, and the
// comark parse step live here. comark is 0.x: calls stay in this file.
import { createMarkdownParser, parseFrontmatter } from "comark";
import toc from "comark/plugins/toc";
import { z } from "zod";
import {
  internalLinkMessage,
  resolveTocAnchor,
  segmentRelativeHref,
  ungroupedSlugs,
} from "./doc-links.ts";
import { mergeOverrides, stripDocPrefix } from "./doc-merge.ts";

// Frontmatter contract for docs/ pages. `order` is a sparse numeric key
// (convention: step by 10, insert between neighbors at the midpoint) — it is
// a value, never a position. summary/features have no consuming page yet;
// their shape follows the plan and the value domain tightens when one lands.
// `internal` marks a repo-internal document: filtered from the docs list,
// it renders on no site surface and takes no part in zh-cn overrides
// (nothing internal is translated); the file stays in docs/en/ for
// repo-side readers.
const frontmatterSchema = z.object({
  title: z.string().min(1),
  description: z.string().min(1),
  order: z.number().nonnegative().optional(),
  summary: z.string().optional(),
  features: z.array(z.string()).optional(),
  // Domain follows common docs-tooling practice; no docs page uses it yet.
  stability: z.enum(["stable", "experimental", "deprecated"]).optional(),
  internal: z.boolean().optional(),
});

export type Frontmatter = z.infer<typeof frontmatterSchema>;

export interface DocEntry {
  slug: string;
  frontmatter: Frontmatter;
  body: string; // markdown without the frontmatter block
  source: string; // full raw markdown, mirrored verbatim under /md/docs/
}

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

// zh-cn translations override the en entry of the same slug wholesale —
// a translation ships as one self-consistent file (frontmatter with body).
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

const { merged: zhMerged, orphans: zhOrphans } = mergeOverrides(
  parsed,
  zhParsed,
);
if (zhOrphans.length > 0) {
  console.warn(
    `[docs] zh-cn entries without an en counterpart: ${zhOrphans.join()}`,
  );
}

// Fixed group order (root docs, then reference/).
// Within a group: order value (absent sorts last), ties by slug. This is the
// single ordering source for the sidebar, the pager, and the index page —
// nothing downstream may re-sort.
const groupOrder = ["docs", "reference"];

function groupRank(slug: string): number {
  const slash = slug.indexOf("/");
  const top = slash === -1 ? "docs" : slug.slice(0, slash);
  const rank = groupOrder.indexOf(top);
  return rank === -1 ? groupOrder.length : rank;
}

function byDocOrder(a: DocEntry, b: DocEntry): number {
  return (
    groupRank(a.slug) - groupRank(b.slug) ||
    (a.frontmatter.order ?? Number.POSITIVE_INFINITY) -
      (b.frontmatter.order ?? Number.POSITIVE_INFINITY) ||
    a.slug.localeCompare(b.slug)
  );
}

// The en canon: the en segment and the locale-independent surfaces (sitemap,
// llms, the /md mirror) all read this list.
export const docs: DocEntry[] = parsed.sort(byDocOrder);
// A slug outside every nav group still renders as a detail page but
// lists in no sidebar or llms group: the build says so at load time.
const ungrouped = ungroupedSlugs(
  docs.map((doc) => doc.slug),
  groupOrder,
);
if (ungrouped.length > 0) {
  console.warn(`[docs] slug has no nav group: ${ungrouped.join()}`);
}

// The zh-cn view: overrides applied over the en set — untranslated slugs
// fall through to the en document — sorted by the same contract.
export const zhDocs: DocEntry[] = zhMerged.sort(byDocOrder);

// Sidebar groups in render order with their members already sorted.
export function docGroups(
  list: readonly DocEntry[] = docs,
): { name: string; entries: DocEntry[] }[] {
  return groupOrder.map((name) => ({
    name,
    entries: list.filter(
      (doc) => groupRank(doc.slug) === groupOrder.indexOf(name),
    ),
  }));
}

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
  fromDir: string,
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
  const slug = stack.join("/").replace(/\.md$/, "");
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
    fromDir,
    anchor ? resolveAnchor(slug, anchor) : "",
  );
}

function rewriteLinks(
  nodes: unknown[],
  fromDir: string,
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
        attrs.href = rewriteHref(attrs.href, fromDir, resolveAnchor);
      }
    }
    rewriteLinks(children, fromDir, ownSlug, resolveAnchor);
  }
}

// Parsed document caches, one per view: each view parses its own entries,
// so a zh-cn override renders its own body, its own TOC, and its own
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
      const slash = doc.slug.indexOf("/");
      rewriteLinks(
        document.nodes as unknown[],
        slash === -1 ? "" : doc.slug.slice(0, slash),
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
