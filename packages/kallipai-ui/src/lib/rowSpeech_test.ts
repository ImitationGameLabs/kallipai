// A jump speaks one sentence, and the sentence is built from what a row is
// called: the cases below read the data (the author, the time) and the
// relationships between the shapes, never the catalog's wording, so a
// translation cannot fail them.

import { assert, assertEquals } from "@std/assert";
import { authorName, jumpAnnouncement } from "./rowSpeech.ts";

Deno.test("a timed jump announcement names its author and its time", () => {
  const spoken = jumpAnnouncement("@alice", "10:00 AM");
  assert(spoken.trim().length > 0, "the jump has to say something");
  assert(spoken.includes("@alice"), "the author belongs in the sentence");
  assert(spoken.includes("10:00 AM"), "so does when the row was said");
});

Deno.test("a row with no time speaks its author alone", () => {
  const untimed = jumpAnnouncement("@alice");
  assert(untimed.trim().length > 0, "never silence");
  assert(untimed.includes("@alice"), "the author still speaks");
});

Deno.test("an untimed row and a timed row do not read alike", () => {
  assert(
    jumpAnnouncement("@alice") !== jumpAnnouncement("@alice", "10:00 AM"),
    "a spoken time has to be audible",
  );
});

Deno.test("an author is their handle when the line carries one", () => {
  assertEquals(authorName("@alice", "a user"), "@alice");
  assertEquals(
    authorName(undefined, "a user"),
    "a user",
    "with no handle the kind's standing name speaks",
  );
  assertEquals(
    authorName("", "an agent"),
    "an agent",
    "an empty handle is no handle at all",
  );
});
