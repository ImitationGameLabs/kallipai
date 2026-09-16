// Docs content pipeline — the single module behind the /docs/ section.
// Loads the repo-root docs/ markdown tree at build time (import.meta.glob
// with ?raw + eager inlines file contents into the bundle), validates
// frontmatter with zod, and owns ordering, neighbor lookup, and the comark
// parse step. comark is 0.x: keeping every comark call inside this file
// confines a renderer swap to one module.
import { createMarkdownParser, parseFrontmatter } from "comark";
import toc from "comark/plugins/toc";
import type { TocLink } from "comark/plugins/toc";
import { z } from "zod";
import { segmentRelativeHref } from "./doc-links.ts";

// Frontmatter contract for docs/ pages. `order` is a sparse numeric key
// (convention: step by 10, insert between neighbors at the midpoint) — it is
// a value, never a position. summary/features have no consuming page yet;
// their shape follows the plan and the value domain tightens when one lands.
const frontmatterSchema = z.object({
  title: z.string().min(1),
  description: z.string().min(1),
  order: z.number().nonnegative().optional(),
  summary: z.string().optional(),
  features: z.array(z.string()).optional(),
  // Domain follows common docs-tooling practice; no docs page uses it yet.
  stability: z.enum(["stable", "experimental", "deprecated"]).optional(),
});

export type Frontmatter = z.infer<typeof frontmatterSchema>;

export interface DocEntry {
  slug: string;
  frontmatter: Frontmatter;
  body: string; // markdown without the frontmatter block
  source: string; // full raw markdown, mirrored verbatim under /md/docs/
}

const rawFiles = import.meta.glob("../../../../docs/**/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

function slugOf(key: string): string {
  return key.replace("../../../../docs/", "").replace(/\.md$/, "");
}

const parsed = Object.entries(rawFiles).map(([key, source]) => {
  const { content, data } = parseFrontmatter(source);
  return {
    slug: slugOf(key),
    frontmatter: frontmatterSchema.parse(data),
    body: content,
    source,
  } satisfies DocEntry;
});

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

export const docs: DocEntry[] = parsed.sort(
  (a, b) =>
    groupRank(a.slug) - groupRank(b.slug) ||
    (a.frontmatter.order ?? Number.POSITIVE_INFINITY) -
      (b.frontmatter.order ?? Number.POSITIVE_INFINITY) ||
    a.slug.localeCompare(b.slug),
);

// Sidebar groups in render order with their members already sorted.
export function docGroups(): { name: string; entries: DocEntry[] }[] {
  return groupOrder.map((name) => ({
    name,
    entries: docs.filter(
      (doc) => groupRank(doc.slug) === groupOrder.indexOf(name),
    ),
  }));
}

export function findDoc(slug: string): DocEntry | undefined {
  return docs.find((doc) => doc.slug === slug);
}

// Neighbors over the same sorted sequence the sidebar renders, so the pager
// and the sidebar can never disagree.
export function neighbors(slug: string): {
  prev: DocEntry | undefined;
  next: DocEntry | undefined;
} {
  const index = docs.findIndex((doc) => doc.slug === slug);
  return {
    prev: index > 0 ? docs[index - 1] : undefined,
    next: index >= 0 && index < docs.length - 1 ? docs[index + 1] : undefined,
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
  // the original href: a live repo link beats a silently broken site one.
  if (!findDoc(slug)) return href;
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

// Parsed document cache. Everything is parsed up front (prerender touches
// every page anyway) so cross-page anchors can be resolved against the
// target's TOC in a second pass.
const parsedDocs = new Map<string, ParsedDoc>();
let parsing: Promise<void> | undefined;

export function parseDoc(entry: DocEntry): Promise<ParsedDoc> {
  parsing ??= (async () => {
    await Promise.all(
      docs.map(async (doc) => parsedDocs.set(doc.slug, await parse(doc.body))),
    );
    for (const doc of docs) {
      const document = parsedDocs.get(doc.slug);
      if (!document) continue;
      const slash = doc.slug.indexOf("/");
      rewriteLinks(
        document.nodes as unknown[],
        slash === -1 ? "" : doc.slug.slice(0, slash),
        doc.slug,
        (slug, anchor) => {
          const target = parsedDocs.get(slug);
          const links = target?.meta.toc?.links as TocLink[] | undefined;
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
          // GitHub keeps punctuation runs as multiple dashes while comark
          // collapses them; compare on a normalized form so both hit.
          const norm = (s: string) => s.replace(/-+/g, "-");
          return (
            ids.find((id) => norm(id) === norm(anchor)) ??
            ids.find((id) => norm(id).endsWith("-" + norm(anchor))) ??
            anchor
          );
        },
      );
    }
  })();
  return parsing.then(() => {
    const document = parsedDocs.get(entry.slug);
    if (!document) throw new Error(`doc not parsed: ${entry.slug}`);
    return document;
  });
}
