// sitemap.xml endpoint: prerendered URL set for every human-facing page in
// both locales, each block pairing the en page with its zh-cn mirror via
// hreflang alternates. The coverage rules live in the pure builder this
// wraps (unit-tested there); the raw-markdown mirrors and the llms.txt
// endpoints stay out — they are transport formats, not pages.
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { docs } from "../../lib/docs.ts";
import { sitemapXml } from "../../lib/sitemap.ts";
import { siteUrl } from "../../lib/site.ts";

export const prerender = true;

export const GET: RequestHandler = () =>
  new Response(
    sitemapXml(
      siteUrl,
      docs.map((doc) => doc.slug),
    ),
    {
      headers: { "content-type": "application/xml; charset=utf-8" },
    },
  );
