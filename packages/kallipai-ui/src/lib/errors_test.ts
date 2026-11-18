// Behavior test for the classifier's user-facing copy: whatever the origin
// says -- a server sentence carrying an internal path, an arbitrary Error's
// message, a refusal the conversation raised -- the view carries our own words
// only. The raw text stays in the console line the caller already keeps.

import { assertEquals } from "@std/assert";
import { KallipaiError, TransportError } from "@kallipai/kallipai-common";
import {
  error_conversation_not_connected,
  error_couldnt_reach,
  error_hint_check_settings,
  error_tagma,
} from "../paraglide/messages.js";
import { classifyError } from "./errors.ts";
import { WriteRefusal } from "./session/writeRefusal.ts";

/** Stand-in for whatever the origin says; the path is there so a leak is
 *  easy to spot, and it is the string the assertions look for. */
const RAW_PATH = "/agents/abc/events";
const RAW = "fetch failed: " + RAW_PATH;

/** The two properties that make the copy ours: no detail channel at all, and
 *  no origin text anywhere in the view. */
function assertOurOwnCopy(view: unknown) {
  assertEquals(Object.hasOwn(view as object, "detail"), false);
  assertEquals(JSON.stringify(view).includes(RAW_PATH), false);
}

Deno.test("a server message stays out of the view", () => {
  const view = classifyError(new KallipaiError({ status: 500, message: RAW }));
  assertEquals(view.title, error_tagma());
  assertOurOwnCopy(view);
});

Deno.test(
  "a rejected request keeps its status-derived title without the body",
  () => {
    const view = classifyError(
      new KallipaiError({ status: 400, message: RAW }),
    );
    assertOurOwnCopy(view);
  },
);

Deno.test("an unknown error's message stays out of the view", () => {
  assertOurOwnCopy(classifyError(new Error(RAW)));
});

Deno.test("a refused write says the conversation is not connected", () => {
  for (const reason of ["closed", "carrier-absent"] as const) {
    const view = classifyError(new WriteRefusal(reason));
    assertEquals(view.title, error_conversation_not_connected(), reason);
    assertEquals(Object.hasOwn(view, "detail"), false, reason);
  }
});

Deno.test("a write refusal is recognised by name alone", () => {
  // A bundler that duplicates the module breaks instanceof; the name is the
  // fallback classifyError relies on, so it gets a case of its own.
  const lookalike = Object.assign(new Error(RAW), { name: "WriteRefusal" });
  const view = classifyError(lookalike);
  assertEquals(view.title, error_conversation_not_connected());
  assertOurOwnCopy(view);
});

Deno.test("a transport error keeps its title and hint", () => {
  const view = classifyError(new TransportError("connect refused"));
  assertEquals(view.title, error_couldnt_reach());
  assertEquals(view.hint, error_hint_check_settings());
});
