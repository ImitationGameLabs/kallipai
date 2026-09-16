// llms.txt endpoint: a static inventory of the docs tree (title, URL,
// one-line description) for LLM-oriented crawlers. Prerendered to
// build/llms.txt by adapter-static; the .txt extension carries the
// text/plain content type through static hosting, mirroring the /md/ form.
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { docGroups } from "../../lib/docs.ts";

export const prerender = true;

const groupLabels: Record<string, string> = {
  docs: "Docs",
  reference: "Reference",
};

export const GET: RequestHandler = () => {
  const sections = docGroups()
    .filter((group) => group.entries.length > 0)
    .map(
      (group) =>
        `## ${groupLabels[group.name] ?? group.name}\n\n` +
        group.entries
          .map(
            (doc) =>
              `- [${doc.frontmatter.title}](/en/docs/${doc.slug}/): ${doc.frontmatter.description}`,
          )
          .join("\n"),
    )
    .join("\n\n");
  const body =
    "# KallipAI\n\n> Documentation for the KallipAI agent runtime.\n\n" +
    sections +
    "\n";
  return new Response(body, {
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
};
