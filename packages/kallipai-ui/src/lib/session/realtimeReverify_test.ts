// reverifySession decision logic: the lesche-401 recovery policy as a pure
// function. Throttle window maps to "skipped" (no verifier call), a surviving
// session to "valid", a dead one to "invalid", an unbound verifier to
// "invalid" (the historical stop-on-401), and a rejecting verifier (network)
// to "valid" (transient, not a rejection).

import { assert, assertEquals } from "@std/assert";
import {
  reverifySession,
  SESSION_REVERIFY_THROTTLE_MS,
  type SessionVerifier,
} from "./realtimeReverify.ts";

Deno.test(
  "reverifySession reports valid when the session survives",
  async () => {
    let calls = 0;
    const verifier: SessionVerifier = () => {
      calls++;
      return Promise.resolve(true);
    };
    const { verdict, verifiedAt } = await reverifySession(verifier, 0, 60_000);
    assertEquals(verdict, "valid");
    assertEquals(verifiedAt, 60_000);
    assertEquals(calls, 1);
  },
);

Deno.test(
  "reverifySession reports invalid when the session is gone",
  async () => {
    const { verdict } = await reverifySession(
      () => Promise.resolve(false),
      0,
      60_000,
    );
    assertEquals(verdict, "invalid");
  },
);

Deno.test(
  "reverifySession skips inside the throttle window without calling the verifier",
  async () => {
    let calls = 0;
    const verifier: SessionVerifier = () => {
      calls++;
      return Promise.resolve(true);
    };
    const verifiedAt = 5000;
    const { verdict, verifiedAt: at } = await reverifySession(
      verifier,
      verifiedAt,
      verifiedAt + SESSION_REVERIFY_THROTTLE_MS - 1,
    );
    assertEquals(verdict, "skipped");
    // A skipped verify must not roll the throttle window forward.
    assertEquals(at, verifiedAt);
    assertEquals(calls, 0);
  },
);

Deno.test(
  "reverifySession verifies again once the throttle window has elapsed",
  async () => {
    let calls = 0;
    const verifier: SessionVerifier = () => {
      calls++;
      return Promise.resolve(true);
    };
    const { verdict } = await reverifySession(
      verifier,
      1000,
      1000 + SESSION_REVERIFY_THROTTLE_MS,
    );
    assertEquals(verdict, "valid");
    assertEquals(calls, 1);
  },
);

Deno.test(
  "reverifySession treats a rejecting verifier (network) as valid-transient",
  async () => {
    const { verdict } = await reverifySession(
      () => Promise.reject(new TypeError("network down")),
      0,
      60_000,
    );
    assertEquals(verdict, "valid");
  },
);

Deno.test(
  "reverifySession falls back to invalid when no verifier is bound",
  async () => {
    const { verdict } = await reverifySession(null, 0, 60_000);
    assertEquals(verdict, "invalid");
  },
);

Deno.test("throttle window is 30s", () => {
  assert(SESSION_REVERIFY_THROTTLE_MS === 30_000);
});
