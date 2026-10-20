// Unit tests for the composer's keystroke decision: the layout convention and
// the IME gate that keeps a composing Enter from sending the message.

import { assertEquals } from "@std/assert";
import { composerKeyIntent } from "./composerKeys.ts";

function key(
  over: Partial<{ key: string; shiftKey: boolean; isComposing: boolean }>,
) {
  return { key: "Enter", shiftKey: false, isComposing: false, ...over };
}

Deno.test("Enter submits at md+, with Shift as the line break", () => {
  assertEquals(composerKeyIntent(key({}), true), "submit");
  assertEquals(composerKeyIntent(key({ shiftKey: true }), true), "none");
});

Deno.test("below md Enter is a newline, never a submit", () => {
  assertEquals(composerKeyIntent(key({}), false), "none");
  assertEquals(composerKeyIntent(key({ key: "a" }), false), "none");
});

Deno.test("a composing key belongs to the IME, whichever key it is", () => {
  assertEquals(composerKeyIntent(key({ isComposing: true }), true), "none");
  assertEquals(
    composerKeyIntent(key({ key: "Escape", isComposing: true }), true),
    "none",
  );
});

Deno.test("Escape asks for the armed quote to be dropped", () => {
  assertEquals(composerKeyIntent(key({ key: "Escape" }), true), "cancel-quote");
  assertEquals(
    composerKeyIntent(key({ key: "Escape" }), false),
    "cancel-quote",
  );
});
