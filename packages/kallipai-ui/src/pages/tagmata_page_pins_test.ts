import { assert, assertMatch } from "@std/assert";

// Source pins for the tagmata page's identity join. The page must derive the
// polis origin it is served by and hand it to joinDeviceRows: the join's
// identity branch fires only for that origin, so dropping the derivation or
// the argument silently returns the panel to the slug convention and splits
// one tagma into two cards again, with the suite green.

const PAGE = new URL("./TagmataPage.svelte", import.meta.url);
const STORE = new URL("../lib/tagmata.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The text of each `callee(...)` call, from the callee name through its
 * closing `);`. Comments go in one alternating pass -- a block comment or a
 * line comment, whichever starts first -- so a mention left in a comment
 * cannot stand in for an argument. */
function callTexts(src: string, callee: string): string[] {
  const code = src.replace(/\/\*[\s\S]*?\*\/|(^|\s)\/\/.*$/gm, "$1");
  const texts: string[] = [];
  for (
    let at = code.indexOf(`${callee}(`);
    at >= 0;
    at = code.indexOf(`${callee}(`, at + 1)
  ) {
    const end = code.indexOf(");", at);
    assert(end > at, `${callee} call is not terminated`);
    texts.push(code.slice(at, end + 2));
  }
  return texts;
}

Deno.test(
  "the tagmata page derives the serving polis origin",
  { permissions: { read: [PAGE] } },
  () => {
    const derived = callTexts(source(PAGE), "$derived").filter((text) =>
      text.includes("archeionPolisOriginOrFail"),
    );
    assert(
      derived.length === 1,
      "exactly one derived value reads the serving polis origin",
    );
    assertMatch(
      derived[0],
      /shellMode\(\)\s*===\s*"online"\s*\?\s*archeionPolisOriginOrFail\(\)\s*:\s*""/,
      "the origin is the serving archeion's, empty when the shell serves no polis",
    );
  },
);

Deno.test(
  "the join receives that origin and compares it on both sides",
  { permissions: { read: [PAGE, STORE] } },
  () => {
    const calls = callTexts(source(PAGE), "joinDeviceRows");
    assert(
      calls.length === 1,
      "the page joins through one joinDeviceRows call",
    );
    assertMatch(
      calls[0],
      /[(,]\s*polisOrigin\s*[,)]/,
      "the join's own argument list carries the page's origin",
    );
    const store = source(STORE);
    assert(
      store.includes("normalizeOrigin(identity.polis) !== origin"),
      "the join compares each reported identity against that origin",
    );
    assert(
      store.includes("const origin = normalizeOrigin(polisOrigin);"),
      "both sides of the comparison go through the same normalizer",
    );
    assert(
      store.includes("new URL(trimmed).origin"),
      "a parseable origin folds to the reading the tagma applies",
    );
  },
);
