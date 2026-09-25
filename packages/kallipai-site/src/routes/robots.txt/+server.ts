// robots.txt endpoint: prerendered to build/robots.txt (the same static
// artifact a static/ file would produce). Generated rather than static so
// the Sitemap line shares the site-wide origin constant with the canonical
// links and the sitemap — one placeholder to replace when the domain lands.
// /md/ is the raw-markdown mirror (duplicate transport of the /docs/ pages)
// and /pagefind/ the search index internals; neither belongs in a crawl
// set. The llms endpoints stay crawlable on purpose: they exist for agents.
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { siteUrl } from "../../lib/site.ts";

export const prerender = true;

export const GET: RequestHandler = () =>
  new Response(
    "User-agent: *\n" +
      "Allow: /\n" +
      "Disallow: /md/\n" +
      "Disallow: /pagefind/\n" +
      "\n" +
      `Sitemap: ${siteUrl}/sitemap.xml\n`,
    { headers: { "content-type": "text/plain; charset=utf-8" } },
  );
