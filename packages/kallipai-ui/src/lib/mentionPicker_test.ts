// Behaviour tests for the mention picker's glue: what a key or a pick moves,
// what an Escape silences, and what a conversation swap keeps or drops. The
// rules are table-tested in mentions_test; this covers the calls between
// them, which is where a change can keep every rule intact and the feature
// broken -- an Escape that never lands, a pick that keeps the token, a cap
// notice outliving the slot it refused.
//
// The module holds runes and `deno test` runs it uncompiled, so it loads behind
// a passthrough `$state` shim plus an ambient declaration (the seam reveal_test
// uses). The controller computes its view on read for that reason: a `$derived`
// would freeze at construction under the shim and test nothing.

import { assertEquals } from "@std/assert";
import type { MentionTarget } from "./room-message.ts";
import { MENTIONS_MAX } from "./room-message.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createMentionPicker } = await import("./mentionPicker.svelte.ts");

function member(id: string, label: string): MentionTarget {
  return { id, kind: "human", handle: `@${label}`, label };
}

const ROSTER: readonly MentionTarget[] = [
  member("id-1", "ada"),
  member("id-2", "adam"),
  member("id-3", "bo"),
];

function key(name: string) {
  return { key: name, isComposing: false, shiftKey: false };
}

/** A picker over a mutable draft and a mutable scope, so a key, a pick and a
 *  route change move it the way the page would. */
function pickerOn(initial = "@ad") {
  let draft = initial;
  let caret = initial.length;
  let scope: string | undefined = "room-1";
  const drops: { start: number; end: number }[] = [];
  const picker = createMentionPicker({
    scope: () => scope,
    members: () => ROSTER,
    draft: () => draft,
    caret: () => caret,
    dropToken: (start, end) => {
      drops.push({ start, end });
      draft = draft.slice(0, start) + draft.slice(end);
      caret = start;
    },
  });
  return {
    picker,
    drops,
    type(text: string) {
      draft = text;
      caret = text.length;
    },
    swapScope(next: string | undefined) {
      scope = next;
    },
  };
}

Deno.test("the popup offers the roster while the caret sits in a token", () => {
  const { picker } = pickerOn("@ad");
  assertEquals(picker.open, true);
  assertEquals(
    picker.rows.map((row) => row.id),
    ["id-1", "id-2"],
  );
  assertEquals(picker.activeIndex, 0);
  assertEquals(picker.activeId, "id-1");
  assertEquals(picker.limitHit, false);
});

Deno.test("Escape silences this token, and typing on opens it again", () => {
  const { picker, type } = pickerOn("@ad");
  assertEquals(picker.key(key("Escape")), true, "the picker owns the Escape");
  assertEquals(picker.open, false);
  assertEquals(picker.key(key("Escape")), false, "a shut popup owns nothing");
  type("@ada");
  assertEquals(picker.open, true, "a new token is a new question");
});

Deno.test("a key outside a token belongs to the composer", () => {
  const { picker } = pickerOn("hello");
  assertEquals(picker.open, false);
  assertEquals(picker.key(key("Enter")), false);
  assertEquals(picker.key(key("ArrowDown")), false);
});

Deno.test(
  "arrows step the active row and Enter takes it, dropping the token",
  () => {
    const { picker, drops } = pickerOn("@ad");
    assertEquals(picker.key(key("ArrowDown")), true);
    assertEquals(picker.activeId, "id-2");
    assertEquals(picker.key(key("Enter")), true);
    assertEquals(picker.picked, ["id-2"]);
    assertEquals(drops, [{ start: 0, end: 3 }], "the token leaves the draft");
    assertEquals(picker.open, false);
  },
);

Deno.test("an accept makes the first row active again", () => {
  const { picker, type } = pickerOn("@a");
  picker.key(key("ArrowDown"));
  assertEquals(picker.activeIndex, 1);
  picker.key(key("Enter"));
  type("@a");
  assertEquals(
    picker.activeIndex,
    0,
    "the cursor starts over for the next token",
  );
});

Deno.test("a repeat pick folds into the list it was already in", () => {
  const { picker } = pickerOn("hello");
  picker.pick("id-1");
  picker.pick("id-1");
  assertEquals(picker.picked, ["id-1"]);
});

Deno.test("a cap refused, freed and settled, and a fresh room", () => {
  const { picker, type, swapScope } = pickerOn("@ad");
  for (let i = 0; i < MENTIONS_MAX; i += 1) picker.pick(`m-${i}`);
  assertEquals(picker.picked.length, MENTIONS_MAX);
  type("@ad");
  picker.pick("one-more");
  assertEquals(picker.picked.length, MENTIONS_MAX, "the cap holds");
  assertEquals(picker.limitHit, true, "and says so on this token");
  picker.remove("m-0");
  assertEquals(picker.picked.length, MENTIONS_MAX - 1);
  assertEquals(picker.limitHit, false, "a freed slot settles the notice");
  picker.pick("m-0");
  type("@ad");
  picker.pick("one-more");
  assertEquals(picker.limitHit, true, "the cap refuses again");
  picker.removeLast();
  assertEquals(picker.limitHit, false, "a spent pick settles the notice too");
  picker.pick("one-more");
  type("@ad");
  picker.pick("one-more-again");
  assertEquals(picker.limitHit, true);
  picker.clear();
  assertEquals(picker.limitHit, false, "a send settles the notice");
  swapScope("room-2");
  type("@ad");
  assertEquals(
    picker.key(key("ArrowDown")),
    true,
    "the active row moves in the fresh room",
  );
  assertEquals(
    picker.limitHit,
    false,
    "a room with no picks carries no cap notice",
  );
});

Deno.test("removeLast drops the newest pick", () => {
  const { picker } = pickerOn("hello");
  picker.pick("id-1");
  picker.pick("id-2");
  picker.removeLast();
  assertEquals(picker.picked, ["id-1"]);
});

Deno.test("clear spends the picks, the dismissal and the cursor", () => {
  const { picker, type } = pickerOn("@ad");
  picker.pick("id-1");
  type("@ad");
  picker.key(key("Escape"));
  assertEquals(picker.open, false);
  picker.clear();
  assertEquals(picker.picked, []);
  assertEquals(picker.open, true, "the dismissal was spent with the picks");
  type("@ada");
  picker.key(key("ArrowDown"));
  assertEquals(picker.activeIndex, 1, "the cursor is one step in");
  picker.clear();
  type("@ada");
  assertEquals(
    picker.activeIndex,
    0,
    "a send makes the first row active again",
  );
});

Deno.test(
  "a conversation swap keeps each room's picks and drops the old" +
    " snapshots",
  () => {
    const { picker, type, swapScope } = pickerOn("@ad");
    picker.pick("id-1");
    type("@ad");
    picker.key(key("Escape"));
    type("@ada");
    picker.key(key("ArrowDown"));
    assertEquals(picker.activeIndex, 1, "the active row sits one step in");
    swapScope("room-2");
    assertEquals(
      picker.picked,
      [],
      "the live room's own picks, and it has none",
    );
    assertEquals(
      picker.open,
      true,
      "the popup is the draft's, not the old room's",
    );
    assertEquals(
      picker.limitHit,
      false,
      "the old cap notice went with the room",
    );
    type("@ad");
    assertEquals(
      picker.key(key("ArrowDown")),
      true,
      "the token the old room silenced answers here",
    );
    assertEquals(
      picker.open,
      true,
      "an Escape there does not silence this room's token",
    );
    assertEquals(
      picker.activeIndex,
      1,
      "the picker starts from the first row, not the old room's place",
    );
    picker.pick("id-2");
    assertEquals(picker.picked, ["id-2"]);
    swapScope("room-1");
    assertEquals(
      picker.picked,
      ["id-1"],
      "the room the user returns to still holds the picks it was given",
    );
    picker.pick("id-3");
    swapScope("room-2");
    picker.clear();
    swapScope("room-1");
    assertEquals(
      picker.picked,
      ["id-1", "id-3"],
      "a send in one room spends only that room's picks",
    );
  },
);

Deno.test("a fresh room reads none of the old one's snapshots", () => {
  const { picker, type, swapScope } = pickerOn("@ad");
  for (let i = 0; i < MENTIONS_MAX; i += 1) picker.pick(`m-${i}`);
  type("@ad");
  picker.pick("one-more");
  assertEquals(picker.limitHit, true, "the cap hangs on this token");
  picker.key(key("ArrowDown"));
  assertEquals(picker.activeIndex, 1, "the active row sits a step in");
  swapScope("room-2");
  // Reads only from here: the popup, the notice and the cursor have to come
  // from this room. The draft and the caret are the same in both, so only
  // the room can tell the snapshots apart.
  assertEquals(
    picker.open,
    true,
    "the token is the reader's, so the popup opens",
  );
  assertEquals(
    picker.limitHit,
    false,
    "the old room's cap notice stayed there",
  );
  assertEquals(picker.activeIndex, 0, "the picker starts at the first row");
  picker.key(key("ArrowDown"));
  assertEquals(
    picker.limitHit,
    false,
    "a write in the new room keeps it settled",
  );
  swapScope("room-1");
  picker.key(key("Escape"));
  assertEquals(
    picker.open,
    false,
    "an Escape silences the token it was typed on",
  );
  swapScope("room-2");
  assertEquals(picker.open, true, "and only that room's token");
});
