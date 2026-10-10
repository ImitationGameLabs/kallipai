// parkedReasonText: one rendering per WireParkedReason variant. The strings
// are hover-only diagnostics; these tests pin every shape so a refactor
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
/** The clock every rendering is pinned against (unix seconds). */
const NOW = 1_700_000_000;

Deno.test("failover chain exhaust renders reason and detail", () => {
  assertEquals(
    parkedReasonText(
      {
        failoverChainExhausted: {
          reason: "no provider",
          detail: "all 3 failed",
        },
      },
      NOW,
    ),
    "failover chain exhausted (no provider): all 3 failed",
  );
});

Deno.test("fatal error renders its message", () => {
  assertEquals(
    parkedReasonText({ fatalError: { message: "boom" } }, NOW),
    "fatal error: boom",
  );
});

Deno.test("token budget renders consumed and budget", () => {
  assertEquals(
    parkedReasonText(
      {
        tokenBudgetExceeded: { consumed: 123_456, budget: 100_000 },
      },
      NOW,
    ),
    "token budget exceeded (123456/100000)",
  );
});

Deno.test("max rounds renders its fixed word", () => {
  assertEquals(parkedReasonText(MAX_ROUNDS, NOW), "max rounds exceeded");
});

Deno.test("transient retry exhaust renders its fixed word", () => {
  assertEquals(
    parkedReasonText(TRANSIENT_RETRY, NOW),
    "transient retries exhausted",
  );
});

Deno.test(
  "quota block renders endpoint, account hint, reset and evidence",
  () => {
    assertEquals(
      parkedReasonText(
        {
          quotaBlocked: {
            endpoint: "openai",
            accountHint: "3f9a1b2c",
            reset: NOW + 2 * 3_600 + 3 * 60,
            detail: "insufficient_quota",
          },
        },
        NOW,
      ),
      "quota blocked (openai · account 3f9a1b2c, reopens in 2h 3min): insufficient_quota",
    );
  },
);

Deno.test("a quota block without a reset hint omits the reopens clause", () => {
  assertEquals(
    parkedReasonText(
      {
        quotaBlocked: {
          endpoint: "openai",
          accountHint: "3f9a1b2c",
          detail: "insufficient_quota",
        },
      },
      NOW,
    ),
    "quota blocked (openai · account 3f9a1b2c): insufficient_quota",
  );
});

Deno.test("a reset hint already past is not rendered as a promise", () => {
  assertEquals(
    parkedReasonText(
      {
        quotaBlocked: {
          endpoint: "openai",
          accountHint: "3f9a1b2c",
          reset: NOW - 1,
          detail: "insufficient_quota",
        },
      },
      NOW,
    ),
    "quota blocked (openai · account 3f9a1b2c): insufficient_quota",
  );
});

// A variant this bundle predates must not be named as a concrete cause the
// bundle cannot back. The object arm is the live case (a newer tagma can add
// a struct variant); the string arm covers a future unit variant.
Deno.test(
  "an unknown object variant renders as unrecognized, with its key",
  () => {
    const future = {
      someFutureReason: { x: 1 },
    } as unknown as WireParkedReason;
    assertEquals(
      parkedReasonText(future, NOW),
      "parked (unrecognized reason: someFutureReason)",
    );
    assertEquals(
      parkedReasonText({} as unknown as WireParkedReason, NOW),
      "parked (unrecognized reason)",
    );
  },
);

Deno.test(
  "an unknown unit variant renders as unrecognized, with its name",
  () => {
    const future = "someFutureUnit" as unknown as WireParkedReason;
    assertEquals(
      parkedReasonText(future, NOW),
      "parked (unrecognized reason: someFutureUnit)",
    );
  },
);
