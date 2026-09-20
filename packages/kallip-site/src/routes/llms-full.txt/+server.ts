// llms-full.txt: the same inventory as llms.txt with every page's full
// markdown body appended, so one fetch carries the whole docs corpus.
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

// The same hierarchy as llms.txt, with each entry's full markdown body
// appended under its list item.
function renderGroup(group: DocGroupNode, depth: number): string {
  const heading = "#".repeat(depth + 1);
  const title = (groupLabels[group.name] ?? group.title) || group.name;
  const parts: string[] = [];
  let lines: string[] = [];
  const flush = () => {
    if (lines.length > 0) parts.push(lines.join("\n\n"));
    lines = [];
  };
  for (const child of group.children) {
    if (child.kind === "entry") {
      const url = `/md/docs/${child.doc.slug}.md`;
      lines.push(
        `- [${child.doc.frontmatter.title}](${url}): ${child.doc.frontmatter.description}\n\n${child.doc.body}`,
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
