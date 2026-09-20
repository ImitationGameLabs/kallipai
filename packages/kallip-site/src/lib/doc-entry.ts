// The docs entry contract, split from docs.ts so the tree module (and
// anything importing it) can reference DocEntry without pulling in the
// vite-only import.meta.glob that docs.ts needs at build time.
import { z } from "zod";

// Frontmatter contract for docs/ pages. `order` is a sparse numeric key
// (convention: step by 10, insert between neighbors at the midpoint) — it is
// a value, never a position. summary/features have no consuming page yet;
// their shape follows the plan and the value domain tightens when one lands.
// `internal` marks a repo-internal document: filtered from the docs list,
// it renders on no site surface and takes no part in zh-cn overrides
// (nothing internal is translated); the file stays in docs/en/ for
// repo-side readers.
export const frontmatterSchema = z.object({
  title: z.string().min(1),
  description: z.string().min(1),
  order: z.number().nonnegative().optional(),
  summary: z.string().optional(),
  features: z.array(z.string()).optional(),
  // Domain follows common docs-tooling practice; no docs page uses it yet.
  stability: z.enum(["stable", "experimental", "deprecated"]).optional(),
  internal: z.boolean().optional(),
});

export type Frontmatter = z.infer<typeof frontmatterSchema>;

export interface DocEntry {
  slug: string;
  // True when this entry is a directory's index page (slug is the
  // directory itself). Link resolution needs the distinction: the URL
  // directory of an index page is the slug itself, while a regular
  // page's is its slug minus the last segment. Set by the tree builder.
  indexPage?: boolean;
  frontmatter: Frontmatter;
  body: string; // markdown without the frontmatter block
  source: string; // full raw markdown, mirrored verbatim under /md/docs/
}
