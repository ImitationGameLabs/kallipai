// parkedReasonText: one rendering per WireParkedReason variant. The strings
// are hover-only diagnostics; these tests pin the five shapes so a refactor
// (or a wire-enum change) cannot silently blur them. Fixtures are inline
// object literals -- the function's parameter type checks their shape.

import { assertEquals } from "@std/assert";
import { parkedReasonText } from "./parkedReason.ts";

Deno.test("failover chain exhaust renders reason and detail", () => {
  assertEquals(
    parkedReasonText({
      failoverChainExhausted: { reason: "no provider", detail: "all 3 failed" },
    }),
    "failover chain exhausted (no provider): all 3 failed",
  );
});

Deno.test("fatal error renders its message", () => {
  assertEquals(
    parkedReasonText({ fatalError: { message: "boom" } }),
    "fatal error: boom",
  );
});

Deno.test("token budget renders consumed and budget", () => {
  assertEquals(
    parkedReasonText({
      tokenBudgetExceeded: { consumed: 123_456, budget: 100_000 },
    }),
    "token budget exceeded (123456/100000)",
  );
});

Deno.test("max rounds renders its fixed word", () => {
  assertEquals(
    parkedReasonText({ maxRoundsExceeded: null }),
    "max rounds exceeded",
  );
});

Deno.test("transient retry exhaust renders its fixed word", () => {
  assertEquals(
    parkedReasonText({ transientRetryExhausted: null }),
    "transient retries exhausted",
  );
});
