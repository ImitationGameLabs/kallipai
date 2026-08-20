// Unit tests for retry.ts: relative-time bucketing edges (same-second,
// boundaries, clock skew) and error-classification precedence (specific
// causes win over the broad network family).

import { assertEquals } from "@std/assert";
import { classifyRetryError, relativeTime } from "./retry.ts";

Deno.test("relativeTime: under a minute reads as just now", () => {
  assertEquals(relativeTime(1000, 1000), { kind: "just", n: 0 });
  assertEquals(relativeTime(1059, 1000), { kind: "just", n: 0 });
});

Deno.test("relativeTime: minute boundaries are inclusive floors", () => {
  assertEquals(relativeTime(1060, 1000), { kind: "min", n: 1 });
  assertEquals(relativeTime(3599, 1000), { kind: "min", n: 43 });
});

Deno.test("relativeTime: hours then days", () => {
  assertEquals(relativeTime(1000 + 3600, 1000), { kind: "hour", n: 1 });
  assertEquals(relativeTime(1000 + 23 * 3600 + 59 * 60, 1000), {
    kind: "hour",
    n: 23,
  });
  assertEquals(relativeTime(1000 + 24 * 3600, 1000), { kind: "day", n: 1 });
  assertEquals(relativeTime(1000 + 3 * 24 * 3600, 1000), { kind: "day", n: 3 });
});

Deno.test("relativeTime: clock skew (future ts) clamps to just now", () => {
  assertEquals(relativeTime(1000, 1200), { kind: "just", n: 0 });
});

Deno.test("classifyRetryError: network family", () => {
  assertEquals(
    classifyRetryError("request failed: connection error"),
    "network",
  );
  assertEquals(classifyRetryError("dns lookup failed"), "network");
});

Deno.test("classifyRetryError: numeric codes match whole tokens only", () => {
  assertEquals(classifyRetryError("port 40122 econnrefused"), "network");
  assertEquals(classifyRetryError("error 14013"), "unknown");
  assertEquals(classifyRetryError("error 4293"), "unknown");
});

Deno.test("classifyRetryError: specific causes take precedence", () => {
  assertEquals(classifyRetryError("connection timed out"), "timeout");
  assertEquals(
    classifyRetryError("error 429: too many requests"),
    "rate_limit",
  );
  assertEquals(classifyRetryError("unauthorized: invalid api key"), "auth");
});

Deno.test("classifyRetryError: unmapped strings read as unknown", () => {
  assertEquals(classifyRetryError("something odd happened"), "unknown");
  assertEquals(classifyRetryError(""), "unknown");
});
