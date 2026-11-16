// Source-read pins for the room session: the wiring a page cannot show by
// compiling. The unread bookkeeping has one owner per room (a lease), and the
// failures these catch are the ones that only appear when two surfaces meet:
// a lost viewing claim, a false read on the room a discussion page left, or a
// page that took the store commands back into its own hands.

import { assert } from "@std/assert";
import { flat } from "../../test_support/flat.ts";

const SESSION = new URL("./roomSession.svelte.ts", import.meta.url);
const PAGE = new URL(
  "../../pages/RoomConversationPage.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the lease enters once and leaves once, whatever the mount order",
  () => {
    const src = flat(source(SESSION));
    assert(
      src.includes("if (hold(key)) unreadStore.enter(key);"),
      "only the first surface in claims the room as viewed",
    );
    assert(
      src.includes("if (before !== null) drop(before);"),
      "a room-id change gives the old lease back",
    );
    assert(
      src.includes("const last = release(key);"),
      "the lease is counted, so the departures' order decides nothing",
    );
    assert(
      src.includes("unreadStore.noteViewedLines(id, lastSeq);"),
      "a surface that drew the room reports its cursor while another still holds it",
    );
    assert(
      src.includes("if (!last) {"),
      "only the last holder ends the viewing claim",
    );
  },
);

Deno.test("the room being left is the one that reports its cursor", () => {
  const src = flat(source(SESSION));
  assert(
    src.includes(
      "const lastSeq = roomConversationsStore.get(id)?.lastSeq ?? null;",
    ),
    "the last seq is read off the room left, not off the one now on screen",
  );
  assert(
    src.includes("unreadStore.leaveRoom(id, lastSeq);"),
    "the last holder reports that cursor when leaving",
  );
  assert(
    src.includes("unreadStore.leaveRoom(id);"),
    "a surface that only drew one discussion leaves without advancing the cursor",
  );
});

Deno.test(
  "a room-id change re-opens the room and gives the old lease back",
  () => {
    const src = flat(source(SESSION));
    const read = src.indexOf("const id = roomId();");
    const untracked = src.indexOf("untrack(() => {");
    assert(
      read !== -1 && untracked !== -1,
      "both the read and the guard are there",
    );
    assert(
      read < untracked,
      "the getter read stays tracked, so the room change reaches this call",
    );
    const opened = src.indexOf("void roomConversationsStore.open(id);");
    const dropped = src.indexOf("if (before !== null) drop(before);");
    assert(opened !== -1 && dropped !== -1, "the open and the drop are here");
    assert(
      dropped < opened,
      "the lease of the room left goes back before the new room opens",
    );
    assert(
      src.includes("held = key;"),
      "the surface remembers the room it holds, so the next change departs from it",
    );
  },
);

Deno.test("stop gives everything back", () => {
  const src = flat(source(SESSION));
  assert(src.includes("stopPoll?.();"), "the poll stops");
  assert(
    src.includes("if (key !== null) untrack(() => drop(key));"),
    "the lease goes back, and the leave writes are untracked",
  );
});

Deno.test("the page has no unread bookkeeping of its own", () => {
  const page = flat(source(PAGE));
  assert(
    page.includes("session.sync();"),
    "the page opens through the session",
  );
  assert(
    page.includes("untrack(() => session.stop());"),
    "the page gives the room back on teardown",
  );
  assert(
    !page.includes("unreadStore.enter(") &&
      !page.includes("unreadStore.leaveRoom("),
    "one owner per room: the page never enters or leaves beside the session",
  );
});
