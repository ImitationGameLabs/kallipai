// Guard for the markdown code white-space invariant: inline <code> must wrap
// (Skeleton pins it to nowrap, pushing prose past the bubble edge), while
// fenced <code> must keep its literal newlines -- Shiki emits each line as a
// span separated by a newline inside <code>, and whitespace-normal collapses
// those into spaces, flattening the whole fence onto one line. No CSS engine
// runs under deno test, so the guard pins the rule pair textually; dropping
// whitespace-pre from the fenced rule is exactly the regression to catch.
import { assert } from "@std/assert";

const css = await Deno.readTextFile(new URL("./markdown.css", import.meta.url));

Deno.test(
  "markdown css: inline code wraps, fenced code preserves newlines",
  () => {
    // Line-start anchor: a future comment quoting the selector must not
    // fake-satisfy the guard -- only a real rule line does.
    const inline = css.match(/(^|\n)\.markdown code \{[^}]*\}/)?.[0] ?? "";
    const fenced = css.match(/(^|\n)\.markdown pre code \{[^}]*\}/)?.[0] ?? "";
    assert(
      inline.includes("whitespace-normal"),
      "inline <code> must stay wrappable (whitespace-normal)",
    );
    assert(
      fenced.includes("whitespace-pre"),
      "fenced <code> must set whitespace-pre or fence newlines collapse to one line",
    );
  },
);
