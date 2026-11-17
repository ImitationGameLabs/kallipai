// Source pins for the room page's mute mark: where it comes from, what it says,
// and what it must not add.
//
//   - the mark is the reader's own standing, read from the same cache the
//     follow control writes, and only for discussions the window draws (the
//     drawn entries are the bound on the reads);
//   - a muted discussion says so in the words a screen reader hears: the icon
//     is decorative, so the entry's name is where the fact lives;
//   - the mark adds nothing else. A count or a line number beside it would turn
//     an indicator into a second summary of the discussion.

import { assert } from "@std/assert";

import { codeLinesOf, source } from "../lib/testing/source_pins.ts";

const ROOM = new URL("./RoomConversationPage.svelte", import.meta.url);

Deno.test("the mark is the reader's muted rule on a drawn discussion", () => {
  const code = codeLinesOf(source(ROOM));
  assert(
    code.includes('threadChoiceOf(roomId, group.root)?.effective === "muted"'),
    "the mark follows the rule in force, not the stored row",
  );
  assert(
    code.includes("const roots = groups.map((group) => group.root);"),
    "only the discussions the window draws are asked about",
  );
  assert(
    code.includes("void ensureThreadChoice(room, root);"),
    "each drawn discussion asks once",
  );
});

Deno.test("a muted discussion says so in the entry's name", () => {
  const code = codeLinesOf(source(ROOM));
  assert(
    code.includes('muted ? sentence + " · " + room_thread_muted() : sentence'),
    "the name carries the word the icon cannot",
  );
});
