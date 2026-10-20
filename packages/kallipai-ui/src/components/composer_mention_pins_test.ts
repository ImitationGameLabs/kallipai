// Source-read pins for the composer's mention seams. Two things can go missing
// without a type error: the key hook's precedence (a picker that never sees
// ArrowDown because the composer's submit rule ran first) and the guards that
// keep the 1:1 composer rendering byte-identical when no snippet is passed.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const COMPOSER = new URL("./Composer.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the mention strips are optional and draw only when passed", () => {
  const src = flat(source(COMPOSER));
  assert(src.includes("mentionBar?: Snippet"), "an optional slot, not a prop");
  assert(src.includes("mentionPicker?: Snippet"), "the popup is a slot too");
  assert(src.includes("{#if mentionBar}"), "absent -> nothing renders");
  assert(src.includes("{#if mentionPicker}"), "absent -> nothing renders");
  assert(src.includes("{@render mentionBar()}"), "the strip is placed");
  assert(src.includes("{@render mentionPicker()}"), "the popup is placed");
});

Deno.test("the page's key hook runs before the composer's own rule", () => {
  const src = flat(source(COMPOSER));
  const hook = src.indexOf("onComposeKey?.(event)");
  const rule = src.indexOf("composerKeyIntent(event, desktop)");
  assert(hook !== -1, "the hook is consulted at all");
  assert(rule !== -1, "the submit/IME rule is still the fallback");
  assert(hook < rule, "a picker key must not be read as submit or cancel");
});

Deno.test("the field announces the popup it drives", () => {
  const src = flat(source(COMPOSER));
  assert(src.includes("aria-expanded="), "expanded state on the field");
  assert(
    src.includes("mentionAria?.open === true"),
    "the expanded state reads the popup's own flag",
  );
  assert(
    src.includes("mentionAria.activeId"),
    "the active row is the popup's active id",
  );
  assert(src.includes("aria-controls="), "the field names its listbox");
  assert(src.includes("aria-activedescendant="), "the active row is announced");
  assert(src.includes("aria-autocomplete="), "list autocomplete shape");
  assert(src.includes('"list"'), "the field advertises a list popup");
});

Deno.test("a caret request places the caret the page asked for", () => {
  const src = flat(source(COMPOSER));
  assert(
    src.includes("setSelectionRange("),
    "accepting a candidate leaves the caret where the token was",
  );
  assert(
    src.includes("composer.caretToken"),
    "the request is keyed, like focus",
  );
});
