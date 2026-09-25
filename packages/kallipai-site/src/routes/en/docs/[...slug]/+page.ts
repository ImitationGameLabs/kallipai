// Docs page data: the slug set comes from the same sorted docs list the
// sidebar renders. The comark document (AST) is parsed once here and shared
// by the page for both rendering and its TOC column.
import { error } from "@sveltejs/kit";
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { EntryGenerator, PageLoad } from "./$types";

import { docs, findDoc, neighbors, parseDoc } from "../../../../lib/docs.ts";

export const entries: EntryGenerator = () =>
  docs.map((doc) => ({ slug: doc.slug }));

// trailingSlash 'always' can feed the rest param with a trailing slash
// when prerender crawls the slash form; normalize before the lookup.
export const load: PageLoad = async ({ params }) => {
  const doc = findDoc(params.slug.replace(/\/$/, ""));
  if (!doc) {
    error(404, "Doc not found");
  }
  const document = await parseDoc(doc);
  const { prev, next } = neighbors(doc.slug);
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
