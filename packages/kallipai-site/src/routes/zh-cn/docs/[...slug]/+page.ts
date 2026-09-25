// zh-cn docs shell: the zh view (zh-cn overrides over the en set, with
// per-slug en fallback), different chrome labels and locale prefix. <html
// lang> is fed by the physical-path hook, which keeps the pagefind indexes
// language-split automatically.
import { error } from "@sveltejs/kit";
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { EntryGenerator, PageLoad } from "./$types";

import { findDoc, neighbors, parseDoc, zhDocs } from "../../../../lib/docs.ts";

export const entries: EntryGenerator = () =>
  zhDocs.map((doc) => ({ slug: doc.slug }));

// trailingSlash 'always' can feed the rest param with a trailing slash
// when prerender crawls the slash form; normalize before the lookup.
export const load: PageLoad = async ({ params }) => {
  const doc = findDoc(params.slug.replace(/\/$/, ""), zhDocs);
  if (!doc) {
    error(404, "Doc not found");
  }
  const document = await parseDoc(doc, zhDocs);
  const { prev, next } = neighbors(doc.slug, zhDocs);
  const brief = (entry: typeof doc) => ({
    slug: entry.slug,
    title: entry.frontmatter.title,
  });
  return {
    doc: brief(doc),
    document,
    tocLinks: document.meta.toc.links,
    prev: prev ? brief(prev) : null,
    next: next ? brief(next) : null,
    // Card tags: the layout promotes these to og:title / og:description.
    seoTitle: doc.frontmatter.title,
    seoDescription: doc.frontmatter.description,
  };
};
