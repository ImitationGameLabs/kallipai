// Pins for a deep link that names a row: the room page lands on it once the
// history lands, and a refresh of the same link does not drag the reader back.
// The landing itself is the reveal controller's (tested with the transcript),
// so what is pinned here is that the page asks for it, for the right row, one
// time per room and row.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the room page takes the row a deep link names",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes(
        "let { roomId, at }: { roomId: string; at?: string } = $props();",
      ),
      "the route hands the page the row it should land on",
    );
    assert(
      src.includes("const landAt = rowParamOf(at);"),
      "the row arrives as text and is read by the rule the pages share",
    );
  },
);

Deno.test(
  "the landing waits for the history and happens once",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes('const key = `${roomId}:${at ?? ""}`;'),
      "one landing per room and row, not one per render",
    );
    assert(
      src.includes("if (landedKey === key || !conv?.historyLoaded) return;"),
      "a refresh of the same link must not pull the reader back, and the row has to be drawn first",
    );
    assert(
      src.includes("reveal.land(landAt)"),
      "the transcript's own reveal is what puts the reader on the row",
    );
  },
);
