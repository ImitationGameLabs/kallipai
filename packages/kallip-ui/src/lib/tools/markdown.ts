// Markdown -> sanitized HTML. `marked` passes raw inline HTML through, so the
// output is run through DOMPurify before any {@html} render — this is the only
// thing keeping LLM-authored assistant content from becoming an XSS vector.
//
// DOMPurify runs with DEFAULTS ONLY. Never add ADD_ATTR/ADD_TAGS here: that
// would re-open the javascript:/on* surface that marked lets through.
//
// Pure, no runes.

import DOMPurify from "dompurify";
import { marked } from "marked";

export function renderMarkdown(src: string): string {
  const html = marked.parse(src) as string;
  // DOMPurify needs a DOM. Consumers are SPA (ssr=false), so a DOM is always
  // present; the typeof guard keeps this kit-agnostic (no $app/environment
  // import) and safe if the module is ever lifted into an SSR context.
  return typeof document !== "undefined"
    ? (DOMPurify.sanitize(html) as string)
    : "";
}
