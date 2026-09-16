// llms-full.txt: the same inventory as llms.txt with every page's full
// markdown body appended, so one fetch carries the whole docs corpus.
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
          .map((doc) => {
            const url = `/md/docs/${doc.slug}.md`;
            return `- [${doc.frontmatter.title}](${url}): ${doc.frontmatter.description}\n\n${doc.body}`;
          })
          .join("\n\n"),
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
