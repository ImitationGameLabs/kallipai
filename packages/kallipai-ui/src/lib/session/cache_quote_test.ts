// The local cache is the one replay path the server cannot rescue: a row it
// holds without a replayed field (quote, attachment) stays that way in the
// transcript, because the window merge keeps the older line. These pins keep
// the fields on the round trip (row -> cache -> transcript) and keep every
// cache write in step.
//
// The module under test is rune-bearing: `deno test` runs it uncompiled, so a
// passthrough $state shim plus an ambient declaration let it load (the same
// seam relayWindow_test uses).

import { assert, assertEquals } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { cachedLineToLine } = await import("./conversation.svelte.ts");
type CachedLine = import("@kallipai/kallipai-keryx-client").CachedLine;

const CONVERSATION = new URL("./conversation.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("a cached line keeps its quote id on the way back", () => {
  const cached = {
    conversationId: "conv-1",
    historyId: 7,
    role: "user",
    text: "quoting you",
    quote: 3,
  } as unknown as CachedLine;
  assertEquals(cachedLineToLine(cached).quote, 3);
  // A row cached before the field existed replays as quote-less, not as 0.
  const legacy = {
    conversationId: "conv-1",
    historyId: 8,
    role: "user",
    text: "older",
  } as unknown as CachedLine;
  assertEquals(cachedLineToLine(legacy).quote, undefined);
});

// Every field a cached row has to hand back to the transcript.
const REPLAYED_FIELDS = ["sender", "createdAt", "attachment", "quote"];

Deno.test(
  "every cache write carries the fields the row replays",
  { permissions: { read: [CONVERSATION] } },
  () => {
    const src = source(CONVERSATION);
    const blocks: string[] = [];
    let at = src.indexOf("cachePut({");
    while (at >= 0) {
      const end = src.indexOf("});", at);
      assert(end > at, "a cachePut call has no close");
      blocks.push(src.slice(at, end));
      at = src.indexOf("cachePut({", end);
    }
    assert(blocks.length >= 3, `expected three write sites: ${blocks.length}`);
    for (const [i, block] of blocks.entries()) {
      for (const field of REPLAYED_FIELDS) {
        assert(
          block.includes(`${field}:`),
          `cache write ${i + 1} drops ${field}: ${block.slice(0, 80)}`,
        );
      }
    }
  },
);
