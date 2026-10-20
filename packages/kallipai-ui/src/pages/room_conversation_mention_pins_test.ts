// Source-read pins for the room page's mention + quote wiring. The behaviour
// lives in lib/mentions.ts (table-driven) and the store; what is left here is
// the page: the strips it hands the composer, the key hook it installs, the
// affordances it renders, and the send options it builds.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the page hands the composer both strips and the key hook", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("{#snippet mentionBar()}"),
    "the picked chips get a slot",
  );
  assert(src.includes("{#snippet mentionPicker()}"), "and so does the popup");
  assert(
    src.includes("<MentionChips") &&
      src.includes("onRemove={(id) => picker.remove(id)}"),
    "a picked chip is removable from the strip",
  );
  assert(
    src.includes("{onComposeKey}") ||
      src.includes("onComposeKey={onComposeKey}"),
    "the picker owns its keys",
  );
  const hook = src.indexOf("function onComposeKey(event: KeyboardEvent)");
  assert(hook !== -1, "the hook is a function, not a leftover binding");
  // A fixed window: a body closes on '}', not on the '});' blockAt seeks.
  const body = src.slice(hook, hook + 320);
  assert(body.includes("picker.key(event)"), "the hook really asks the picker");
  assert(
    body.includes("event.preventDefault()"),
    "and stops the character the key would otherwise type",
  );
});

Deno.test("the field is told what the popup is showing", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("mentionAria={{");
  assert(at !== -1, "the page hands the composer one aria object");
  // A fixed window too: an object literal closes on '}', not on '});'.
  const block = src.slice(at, at + 160);
  assert(block.includes("open: picker.open"), "the popup's own flag");
  assert(
    block.includes("activeId: picker.activeId === undefined") &&
      block.includes("mentionOptionId(picker.activeId)"),
    "the active row is named by the option id the popup rendered",
  );
});

Deno.test("a send spends both strips and hands them over", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("send: (text) => {");
  assert(at !== -1, "the page builds the send closure");
  const block = src.slice(at, at + 420);
  assert(block.includes("const mentions = picker.picked"), "the picks");
  assert(block.includes("const quote = armedSeq"), "only this room's seq");
  assert(block.includes("picker.clear()"), "the picks are spent");
  assert(block.includes("pendingQuote = null"), "and so is the quote");
});

Deno.test("the page keeps the keyboard state machine out of itself", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("createMentionPicker("),
    "the page owns state, the module owns the rules",
  );
  assert(
    !src.includes("lastIndexOf("),
    "no hand-rolled token scan in the component",
  );
});

Deno.test("a send carries both the picks and the quote", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes(
      "roomConversationsStore.send(roomId, text, { mentions, quote })",
    ),
    "one send, one options object",
  );
});

Deno.test(
  "every confirmed room row can be quoted, an optimistic one cannot",
  () => {
    const src = flat(source(PAGE));
    assert(src.includes("function quotable(line: RoomLine)"), "one predicate");
    const at = src.indexOf("function quotable(line: RoomLine)");
    const body = src.slice(at, at + 200);
    assert(body.includes("line.seq > 0"), "a synthetic seq names no row");
    assert(
      body.includes("!roomLinePending(line)"),
      "an in-flight row is not quotable",
    );
    assert(body.includes("!line.failed"), "nor is a failed one");
  },
);

Deno.test("the page renders the quoting affordances on each row", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("<RowQuoteAffordance"),
    "the shared quote button and its menu",
  );
  assert(
    src.includes("menuOpen={rowActions.isMenuOpen(line)}"),
    "one open menu per transcript, held by the row controller",
  );
  assert(
    src.includes("onQuote={() => rowActions.quote(line)}"),
    "the row's quote goes through the same controller",
  );
  assert(src.includes("<QuoteBar"), "the armed quote gets its strip");
  assert(src.includes("<MentionPicker"), "the candidate popup is rendered");
});
