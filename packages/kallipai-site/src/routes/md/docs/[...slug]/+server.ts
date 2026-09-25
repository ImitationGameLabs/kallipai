// Markdown mirror: /md/docs/<slug>.md - a file-style URL
// carrying the .md extension, hosted by this independent md/docs route segment
// so it never competes with the /docs/ page route for the same path (a same-
// slug +server.ts is impossible in SvelteKit: one path cannot hold both a
// page and a server endpoint).
// Why the URL must end in .md: adapter-static writes each prerendered +server
// response body to the file named by the route path, and response headers do
// not survive static hosting — the content type comes from the file extension
// at the serving layer. A directory-style URL (trailing slash + index.html)
// would label the markdown as text/html.
// trailingSlash 'always' interaction: for paths whose last segment carries an
// extension, SvelteKit does not append a trailing slash (verified in this
// repo: the prerendered mirror lands at build/md/docs/<slug>.md and no 308
// redirect is emitted for the bare extension URL).
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { docs, findDoc } from "../../../../lib/docs.ts";

export const prerender = true;

export const entries = () => docs.map((doc) => ({ slug: `${doc.slug}.md` }));

export const GET: RequestHandler = ({ params }) => {
  const doc = findDoc(params.slug.replace(/\.md$/, ""));
  if (!doc) {
    return new Response(null, { status: 404 });
  }
  return new Response(doc.source, {
    headers: { "content-type": "text/markdown; charset=utf-8" },
  });
};
