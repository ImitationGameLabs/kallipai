// retryFormat: outcome wording, the absolute/relative retry lines, and the
// classified-error display labels for the agent detail retry card. These
// tests pin the line shapes and bucket boundaries the card renders (en is
// the paraglide default in tests, matching profiles-view_test).

import { assertEquals } from "@std/assert";
import {
  fmtAbsoluteRetry,
  fmtRelativeRetry,
  retryOutcome,
} from "./retryFormat.ts";

const base = {
  timestamp: 1_000_000,
  attempt: 2,
  max_attempts: 3,
  error: "boom",
};

Deno.test("retryOutcome reads retried until the attempt hits the cap", () => {
  assertEquals(
    retryOutcome({ ...base, attempt: 2, max_attempts: 3 }),
    "retried",
  );
  assertEquals(
    retryOutcome({ ...base, attempt: 3, max_attempts: 3 }),
    "exhausted",
  );
});

Deno.test(
  "fmtAbsoluteRetry shows the raw error; date renders non-empty",
  () => {
    const line = fmtAbsoluteRetry(base);
    assertEquals(line.endsWith(" — boom (retried)"), true);
    assertEquals(line.split(" — ")[0].length > 0, true);
  },
);

Deno.test("fmtRelativeRetry buckets against the injected clock", () => {
  const now = 1_000_000;
  // Same second reads as "just now" -- so does clock skew (future ts).
  assertEquals(
    fmtRelativeRetry({ ...base, timestamp: now }, now),
    "just now — error (retried)",
  );
  assertEquals(
    fmtRelativeRetry({ ...base, timestamp: now + 100 }, now),
    "just now — error (retried)",
  );
  // 59s still "just", 60s tips into minutes.
  assertEquals(
    fmtRelativeRetry({ ...base, timestamp: now - 59 }, now),
    "just now — error (retried)",
  );
  assertEquals(
    fmtRelativeRetry({ ...base, timestamp: now - 60 }, now),
    "1 min ago — error (retried)",
  );
});

Deno.test(
  "fmtRelativeRetry classifies the error and carries the outcome",
  () => {
    const now = 1_000_000;
    assertEquals(
      fmtRelativeRetry(
        {
          timestamp: now - 300,
          attempt: 2,
          max_attempts: 3,
          error: "connection refused",
        },
        now,
      ),
      "5 min ago — connection error (retried)",
    );
    assertEquals(
      fmtRelativeRetry(
        {
          timestamp: now - 7_200,
          attempt: 2,
          max_attempts: 3,
          error: "read timeout",
        },
        now,
      ),
      "2 h ago — timeout (retried)",
    );
    assertEquals(
      fmtRelativeRetry(
        {
          timestamp: now - 3 * 86_400,
          attempt: 3,
          max_attempts: 3,
          error: "nope",
        },
        now,
      ),
      "3 d ago — error (exhausted)",
    );
  },
);
