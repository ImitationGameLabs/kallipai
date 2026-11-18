// Behavior test for the classifier's user-facing copy: whatever the origin
// says -- a server sentence carrying an internal path, an arbitrary Error's
// message, a refusal the conversation raised -- the view carries our own words
// only. The raw text stays in the console line the caller already keeps.

import { assertEquals } from "@std/assert";
import {
  DataPlaneApiError,
  KallipaiError,
  TransportError,
} from "@kallipai/kallipai-common";
import {
  chat_send_failed,
  error_conversation_not_connected,
  error_couldnt_reach,
  error_hint_check_settings,
  error_tagma,
} from "../paraglide/messages.js";
import { classifyError, sendFailureCopy } from "./errors.ts";
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

Deno.test("a failed send carries our copy and its code", () => {
  const failure = sendFailureCopy(
    new KallipaiError({
      status: 409,
      message: RAW,
      code: "profile_set_unusable",
    }),
  );
  assertEquals(failure.copy, chat_send_failed());
  assertEquals(failure.code, "profile_set_unusable");
  assertEquals(JSON.stringify(failure).includes(RAW_PATH), false);
});

Deno.test("a relay rejection carries our copy and its code", () => {
  const failure = sendFailureCopy(
    new DataPlaneApiError(429, RAW, "rate_limited"),
  );
  assertEquals(failure.copy, chat_send_failed());
  assertEquals(failure.code, "rate_limited");
  assertEquals(JSON.stringify(failure).includes(RAW_PATH), false);
});

Deno.test("a rejection frame carries our copy and its code", () => {
  // The relay answers a send with an error frame, a plain object rather
  // than a thrown class: it still names its code, so the line keeps the
  // short form keyed by that code.
  const failure = sendFailureCopy({
    kind: "error",
    req_id: "r-1",
    code: "peer_unavailable",
    message: RAW,
  });
  assertEquals(failure.copy, chat_send_failed());
  assertEquals(failure.code, "peer_unavailable");
  assertEquals(JSON.stringify(failure).includes(RAW_PATH), false);
});

Deno.test("an untyped send failure carries our copy with no code", () => {
  const failure = sendFailureCopy(new Error(RAW));
  assertEquals(failure.copy, chat_send_failed());
  assertEquals(failure.code, undefined);
  assertEquals(JSON.stringify(failure).includes(RAW_PATH), false);
});
