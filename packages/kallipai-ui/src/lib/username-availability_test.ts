import { assert, assertEquals, assertFalse } from "@std/assert";
import {
  USERNAME_AVAILABILITY_DEBOUNCE_MS,
  foldStatus,
  isStale,
  shouldProbe,
} from "./username-availability.ts";

Deno.test("probe arms only for locally valid handles", () => {
  assert(shouldProbe("alice-doe"));
  assertFalse(shouldProbe("ab")); // too short
  assertFalse(shouldProbe("no_underscore"));
  assertFalse(shouldProbe("has space"));
});

Deno.test("fold maps the three renderable statuses to done", () => {
  assertEquals(foldStatus("available"), {
    phase: "done",
    status: "available",
  });
  assertEquals(foldStatus("taken"), { phase: "done", status: "taken" });
  assertEquals(foldStatus("reserved"), { phase: "done", status: "reserved" });
});

Deno.test("fold maps invalid to idle (no fourth render state)", () => {
  assertEquals(foldStatus("invalid"), { phase: "idle" });
});

Deno.test("stale tokens are exactly the non-latest ones", () => {
  assertFalse(isStale(3, 3));
  assert(isStale(2, 3));
});

Deno.test("debounce constant is the operator-specified 3 seconds", () => {
  assertEquals(USERNAME_AVAILABILITY_DEBOUNCE_MS, 3000);
});
