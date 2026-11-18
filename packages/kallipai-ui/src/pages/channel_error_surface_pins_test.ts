import { assert } from "@std/assert";

// Wiring pins for the channel page's error surface, in the offline_view_pins
// discipline: a classifier that is imported but never reaches the error line
// renders nothing on the relay surface, and the whole suite stays green. The
// pins also lock the single-message rule: while the link's own copy is up,
// the composer must not repeat "not connected" over it.
const PAGE = new URL("./ChannelChatPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

const src = source(PAGE);

Deno.test("the page classifies the link failure into our own copy", () => {
  assert(src.includes('import { classifyError } from "../lib/errors.ts";'));
  assert(src.includes("const linkError = $derived(conv?.link.error ?? null);"));
  assert(src.includes("linkError && !isWriteRefusal(linkError)"));
  assert(src.includes("? classifyError(linkError).title"));
});

Deno.test("the error line and its status ride the link failure", () => {
  assert(src.includes("conv?.transcript.error ?? linkErrorCopy ?? undefined"));
  assert(src.includes('conv?.transcript.status === "error" || linkErrorCopy'));
  assert(src.includes("status={errorStatus}"));
  assert(src.includes("error={errorLine}"));
});

Deno.test("the generic notice stands down while the error line is up", () => {
  const start = src.indexOf("const disabledNotice");
  const end = src.indexOf("\n  );", start);
  const block = src.slice(start, end);
  assert(block.includes(": linkErrorCopy"));
  assert(block.includes("? undefined"));
  assert(block.indexOf("linkErrorCopy") < block.indexOf("not-connected"));
});
