// The preview calls its rows the start of a discussion. Nothing in `threads.ts`
// states why that is true any more: the gate used to be the window's catch-up,
// and it is a fact about how the room window is filled. Three premises carry
// it -- the walk forwards only, a live row arrives with no room number, and the
// confirmed set keeps numbered rows alone -- so the claim "these are the first"
// is a claim about the walk, not about the window. Any one of them changing
// would let a stretch of the middle be drawn as the beginning with nothing
// failing on the way, which is why they are pinned here rather than described.
//
// The window's own half is pinned where it is implemented: a request with no
// cursor starts at the room's first row (crates/platform/kallipai-lesche/src/
// chat/rooms/tests.rs:653-667, the case that asks for a page of no cursor).

import { assert } from "@std/assert";
import { source } from "./testing/source_pins.ts";

const STORE = new URL("./session/roomConversations.svelte.ts", import.meta.url);
const THREADS = new URL("./threads.ts", import.meta.url);

Deno.test("the room walk moves forward and never pages backwards", () => {
  const src = source(STORE);
  assert(
    src.includes("afterSeq: conv.lastSeq ?? undefined"),
    "a page asks for what follows the last row it holds",
  );
  assert(
    !src.includes("beforeSeq"),
    "nothing in the room window pages backwards",
  );
});

Deno.test("a live row arrives with no room number", () => {
  const src = source(STORE);
  assert(
    src.includes(
      "this.renderPublic(conv, ciphertextB64, sender, /*seq*/ null);",
    ),
    "a delivery hands the store a row with no number to place it by",
  );
  assert(
    src.includes("seq: seq ?? conv.nextSyntheticSeq()"),
    "an unnumbered row gets a local negative number, below every room number",
  );
});

Deno.test("the confirmed set is numbered rows alone", () => {
  assert(
    source(THREADS).includes("line.seq >= 0 && !line.failed"),
    "an unnumbered or failed row is not a reply the room holds",
  );
});
