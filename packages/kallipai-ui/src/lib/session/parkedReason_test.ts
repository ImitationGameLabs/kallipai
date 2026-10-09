// parkedReasonText: one rendering per WireParkedReason variant. The strings
// are hover-only diagnostics; these tests pin the five shapes so a refactor
// (or a wire-enum change) cannot silently blur them. Fixtures mirror
// the wire bytes: struct variants as single-key objects, unit variants
// as bare strings (serde's externally tagged default).
// Unit-variant fixtures live as top-level annotated constants: deno's
// test-file checker mis-resolves bare literal arguments inside Deno.test
// callbacks for cross-package union parameters.

import { assertEquals } from "@std/assert";
import type { WireParkedReason } from "@kallipai/kallipai-client";
import { parkedReasonText } from "./parkedReason.ts";
const MAX_ROUNDS: WireParkedReason = "maxRoundsExceeded";
const TRANSIENT_RETRY: WireParkedReason = "transientRetryExhausted";

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
  assertEquals(parkedReasonText(MAX_ROUNDS), "max rounds exceeded");
});

Deno.test("transient retry exhaust renders its fixed word", () => {
  assertEquals(
    parkedReasonText(TRANSIENT_RETRY),
    "transient retries exhausted",
  );
});
