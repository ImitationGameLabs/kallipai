// sitemap.xml endpoint: prerendered URL set for every human-facing page in
// both locales, each docs page pairing with its zh-cn mirror only when
// the zh tree carries the translation, via hreflang alternates.
// wraps (unit-tested there); the raw-markdown mirrors and the llms.txt
// endpoints stay out — they are transport formats, not pages.
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { docs, zhDocs } from "../../lib/docs.ts";
import { sitemapXml } from "../../lib/sitemap.ts";
import { siteUrl } from "../../lib/site.ts";

export const prerender = true;

export const GET: RequestHandler = () =>
  new Response(
    sitemapXml(
      siteUrl,
      docs.map((doc) => doc.slug),
      zhDocs.map((doc) => doc.slug),
    ),
    {
      headers: { "content-type": "application/xml; charset=utf-8" },
    },
  );
