// llms.txt endpoint: a static inventory of the docs tree (title, URL,
// one-line description) for LLM-oriented crawlers. Prerendered to
// build/llms.txt by adapter-static; the .txt extension carries the
// text/plain content type through static hosting, mirroring the /md/ form.
// $types is a SvelteKit-generated virtual module (no .ts on disk), so the
// sloppy-import rule can never see a real extension to accept.
// deno-lint-ignore no-sloppy-imports
import type { RequestHandler } from "./$types";

import { docGroups, docs } from "../../lib/docs.ts";
import type { DocGroupNode } from "../../lib/docs.ts";

export const prerender = true;

const groupLabels: Record<string, string> = {
  docs: "Docs",
  "harness-design": "Harness Design",
  deployment: "Deployment",
  configuration: "Configuration",
  reference: "Reference",
};

// Group hierarchy renders as heading levels: a top-level group is `##`,
// each subgroup one level deeper. Entries link their page URLs (the
// index page is a regular entry under its group).
function renderGroup(group: DocGroupNode, depth: number): string {
  const heading = "#".repeat(depth + 1);
  const title = (groupLabels[group.name] ?? group.title) || group.name;
  const parts: string[] = [];
  let lines: string[] = [];
  const flush = () => {
    if (lines.length > 0) parts.push(lines.join("\n"));
    lines = [];
  };
  for (const child of group.children) {
    if (child.kind === "entry") {
      lines.push(
        `- [${child.doc.frontmatter.title}](/en/docs/${child.doc.slug}/): ${child.doc.frontmatter.description}`,
      );
    } else {
      flush();
      parts.push(renderGroup(child.node, depth + 1));
    }
  }
  flush();
  return [`${heading} ${title}`, ...parts].filter(Boolean).join("\n\n");
}

export const GET: RequestHandler = () => {
  const sections = docGroups(docs)
    .filter((group) => group.entries.length > 0 || group.groups.length > 0)
    .map((group) => renderGroup(group, 1))
    .join("\n\n");
  const body =
    "# KallipAI\n\n> Documentation for the KallipAI agent runtime.\n\n" +
    sections +
    "\n";
  return new Response(body, {
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
};
