// The discussion's reading surface and its quote path: a row's action row
// offers the quote control, the discussion holds the armed row itself, and
// the bar above the composer is the shared one. A type-check cannot see that
// the discussion keeps the quote (rather than the page), nor that the reply
// carries it out.

import { assert } from "@std/assert";
import { flat } from "../../test_support/flat.ts";

const CONVERSATION = new URL("./ThreadConversation.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("a quotable reply arms the discussion's own quote", () => {
  const src = flat(source(CONVERSATION));
  assert(src.includes("<QuoteButton"), "the row offers the control");
  assert(
    src.includes("onclick={() => armQuote(line)}"),
    "the discussion holds the armed row",
  );
  assert(
    src.includes("{#if armedQuote !== undefined}"),
    "the bar is drawn only while something is armed",
  );
  assert(
    src.includes("<QuoteBar") &&
      src.includes("preview={previewOf(armedQuote)}"),
    "the shared bar states who and what is quoted",
  );
});

Deno.test(
  "the reply carries the armed row, and only that quote clears after it",
  () => {
    const src = flat(source(CONVERSATION));
    assert(
      src.includes("const sent = armedQuote;"),
      "the send keeps the quote it started with",
    );
    assert(
      src.includes("await onReply(text, sent?.seq);"),
      "the seq travels with the text",
    );
    // The composer clears the draft on submit (its own contract, tested
    // there); what the discussion owes is that the quote goes only once the send
    // it carried has answered.
    const after = src.slice(src.indexOf("await onReply(text, sent?.seq);"));
    assert(
      after.includes("armedQuote = undefined;"),
      "a sent reply leaves no quote armed for the next one",
    );
    assert(
      after.includes("if (armGeneration === sentGeneration) {") &&
        after.includes("if (sent !== undefined) onArmSettled?.();"),
      "only the quote that was sent is spent, and the page hears it",
    );
  },
);

Deno.test("arming and dropping each bump the discussion's generation", () => {
  const src = flat(source(CONVERSATION));
  for (const fn of ["function armQuote(", "function disarmQuote("]) {
    const from = src.indexOf(fn);
    assert(from >= 0, `${fn} is in the discussion`);
    assert(
      src.slice(from, src.indexOf("}", from)).includes("armGeneration += 1;"),
      `${fn} bumps the generation, so a send cannot spend an arm it never carried`,
    );
  }
});
