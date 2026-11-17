// Source pins for the follow control: four shapes a type-check cannot see.
//
//   - the eye reads the rule and a screen reader hears the click: two keys from
//     one table, so the button cannot promise a state in one voice and another
//     in the other;
//   - nothing is drawn before a standing is known: a control that painted a
//     rule before reading one would be announcing the default of the wrong
//     reader;
//   - a click is held while its write is out, and the state shown afterwards is
//     the one the route answered (the outcome of dropping a choice belongs to
//     the caller's identity, so guessing it is the one mistake this control
//     exists to prevent);
//   - a failure stays with the thread that met it, never following the reader
//     into the next thread the panel opens.

import { assert } from "@std/assert";

import { codeLinesOf, source } from "../testing/source_pins.ts";

const CONTROL = new URL(
  "../../components/rooms/ThreadFollowControl.svelte",
  import.meta.url,
);

Deno.test(
  "the visible words and the accessible name come from one table",
  () => {
    const text = source(CONTROL);
    assert(
      text.includes("aria-label={`${display.text()} · ${display.action()}`}"),
      "the name starts with the rule on screen, then names the click",
    );
    assert(
      text.includes("{display.text()}"),
      "the visible words say the rule in force",
    );
  },
);

Deno.test("no rule is drawn before one is known", () => {
  assert(
    source(CONTROL).includes("{#if display !== null}"),
    "an unread standing draws nothing",
  );
});

Deno.test("a click is held while its write is out", () => {
  const code = codeLinesOf(source(CONTROL));
  assert(
    code.includes("disabled={busy}"),
    "the button cannot be clicked twice",
  );
  const started = code.indexOf("busy = true;");
  const write = code.indexOf("await setThreadChoice");
  const drop = code.indexOf("await clearThreadChoice");
  assert(started >= 0 && write > started && drop > started);
});

Deno.test("a failure stays with the thread that met it", () => {
  const code = codeLinesOf(source(CONTROL));
  assert(code.includes("failedIn = key;"), "the failure names its thread");
  assert(
    code.includes("failedIn === key"),
    "the line is drawn only on the thread that failed",
  );
});
