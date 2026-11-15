// Source-read pins for the room send path's thread: the three write sites that
// must agree, plus the read path that keeps one discussion. A thread is settled
// once (so the optimistic line and the wire carry the same root), the optimistic
// line carries it, the wire carries it, and a retry re-sends it -- dropping it
// there would turn a retried reply into a new topic.

import { assert } from "@std/assert";
import { flat } from "../../test_support/flat.ts";

const STORE = new URL("./roomConversations.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the store settles a thread the way it settles mentions", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("const thread = checkedThread(options?.thread)"),
    "one settle point, before the optimistic line is drawn",
  );
});

Deno.test("the optimistic line carries the discussion it joins", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("...threadField(thread)"),
    "the bubble shows as a reply from the instant it appears",
  );
});

Deno.test("the payload carries the declared thread", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("encodeRoomSendMessage(trimmed, { mentions, quote, thread })"),
    "the wire gets the same root the optimistic line drew",
  );
});

Deno.test("a retry re-sends the thread with the text", () => {
  const src = flat(source(STORE));
  assert(src.includes("thread: line.thread"), "a retried reply stays a reply");
});

Deno.test("the thread read keeps the root and the rows filed under it", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("if (row.seq !== root && thread !== root) continue;"),
    "one discussion's rows, and nothing else",
  );
  assert(
    src.includes("const thread = row.thread ?? decoded.thread ?? null;"),
    "the stored fact first, the payload's declaration as the fallback",
  );
});
