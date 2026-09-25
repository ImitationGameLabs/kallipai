import { assertEquals, assertRejects } from "@std/assert";
import {
  addPasskey,
  adminLoginWithKey,
  classifyRegisterConflict,
  completeOAuth,
  completeOAuthSignup,
  loginWithDiscoverablePasskey,
} from "./auth.ts";
import { ArcheionApiError } from "./types.ts";
import type { ArcheionClient } from "./http.ts";

// `classifyRegisterConflict` is the only code in this package whose
// correctness depends on EXACT string matching against the archeion's 409 prose
// (crates/platform/kallipai-archeion/src/routes/auth.rs: "username already taken").
// Registration collects no email, so username is the only collision. These
// tests pin that contract: a server-side copy change must update the switch or
// fail here.

Deno.test(
  "classifyRegisterConflict maps the username-collision message",
  () => {
    assertEquals(
      classifyRegisterConflict("username already taken"),
      "duplicate-username",
    );
  },
);

Deno.test("classifyRegisterConflict falls back to unknown on drift", () => {
  // If the archeion rephrases the message, surface it as "unknown" (generic
  // failure copy) rather than silently misclassifying -- this is the drift
  // signal.
  assertEquals(classifyRegisterConflict("something else entirely"), "unknown");
});

// --- driver coverage ------------------------------------------------------
// The drivers below take an `ArcheionClient` and call `navigator.credentials`. The
// client is mocked (only the methods each driver exercises); `navigator.
// credentials.get` is stubbed via Object.defineProperty and restored after each
// test. These pin the load-bearing branches the page relies on: the
// discoverable driver's conditional-mediation placement, and the credential-
// agnostic add-passkey step-up (OAuth-only -> reauth-required; discoverable opt
// threading).

/** A mock ArcheionClient carrying only the methods a given driver exercises. Cast
 *  through `unknown` so each test implements just what it needs. */
type MockClient = Partial<ArcheionClient>;
const asClient = (m: MockClient): ArcheionClient =>
  m as unknown as ArcheionClient;

/** Temporarily replace `navigator.credentials.get` with `impl`, returning a
 *  restore function. Captures every options object it is called with. Deno's
 *  test runtime does not always expose `navigator.credentials`, so a minimal
 *  container is installed first when absent. */
function stubCredentialGet(impl: (opts: unknown) => Promise<unknown>): {
  restore: () => void;
  calls: unknown[];
} {
  const calls: unknown[] = [];
  const wrapper = (opts: unknown): Promise<unknown> => {
    calls.push(opts);
    return impl(opts);
  };
  if (!navigator.credentials) {
    Object.defineProperty(navigator, "credentials", {
      value: {},
      configurable: true,
      writable: true,
    });
  }
  const target = navigator.credentials as unknown as Record<string, unknown>;
  const original = target.get;
  Object.defineProperty(target, "get", {
    value: wrapper,
    configurable: true,
    writable: true,
  });
  return {
    calls,
    restore: () => {
      Object.defineProperty(target, "get", {
        value: original,
        configurable: true,
        writable: true,
      });
    },
  };
}

Deno.test(
  "loginWithDiscoverablePasskey places mediation on the outer get",
  async () => {
    // `mediation: "conditional"` MUST ride on the outer `credentials.get` call,
    // not inside `publicKey` (the server model drops it from RequestChallenge-
    // Response). A regression that moves it inside `optionsForGet` would silently
    // turn conditional autofill into a full-picker get on every browser.
    const client = asClient({
      loginDiscoverableBegin: () =>
        Promise.resolve({
          ceremony_id: "c",
          options: { publicKey: { challenge: "" } },
        }),
      loginDiscoverableFinish: () => Promise.resolve({ user_id: "u" }),
    });
    // Return null (user dismissed) so the driver does not reach a real finish.
    const stub = stubCredentialGet(() => Promise.resolve(null));
    try {
      await loginWithDiscoverablePasskey(client);
    } finally {
      stub.restore();
    }
    assertEquals(stub.calls.length, 1);
    const opts = stub.calls[0] as { mediation?: string; publicKey?: unknown };
    assertEquals(opts.mediation, "conditional");
    assertEquals(opts.publicKey !== undefined, true);
  },
);

Deno.test(
  "addPasskey with no username (OAuth-only) returns reauth-required",
  async () => {
    // An OAuth-only account has no passkey to step up with. The driver must
    // return `reauth-required` on a 403 WITHOUT calling navigator.credentials or
    // a second begin. `beginCalls` proves begin ran exactly once (the failing
    // one) and was not retried.
    let beginCalls = 0;
    const client = asClient({
      addPasskeyBegin: () => {
        beginCalls += 1;
        return Promise.reject(new ArcheionApiError(403, "reauth-required"));
      },
    });
    const result = await addPasskey(client, { label: "Phone" });
    assertEquals(result, { ok: false, reason: "reauth-required" });
    assertEquals(beginCalls, 1);
  },
);

Deno.test("addPasskey threads the discoverable opt into begin", async () => {
  // The discoverable opt must reach the FIRST `addPasskeyBegin` (the same
  // `beginOpts` object is what the step-up retry reuses, so a regression that
  // drops it from the first call would also drop it from the retry and enroll
  // the wrong credential kind). This asserts the first call sees it; the retry
  // path itself needs a full mock + credential and is not exercised here.
  const beginOptsSeen: unknown[] = [];
  const client = asClient({
    addPasskeyBegin: (opts?: { discoverable?: boolean }) => {
      beginOptsSeen.push(opts);
      return Promise.reject(new ArcheionApiError(403, "reauth-required"));
    },
  });
  await addPasskey(client, {
    username: "alice",
    label: "Phone",
    discoverable: true,
  });
  assertEquals(beginOptsSeen, [{ discoverable: true }]);
});

// --- OAuth finish / signup-complete drivers -------------------------------
// `completeOAuth` must branch on the 202 needs-username body (an unlinked
// identity -> the SPA collects a username); `completeOAuthSignup` must map the
// archeion's 409 "username already taken" to `duplicate-username` (the same prose
// the passkey register flow pins above). Both pin the wire contract.

Deno.test("completeOAuth maps a 202 needs-username body", async () => {
  const client = asClient({
    oauthFinish: () =>
      Promise.resolve({
        kind: "needs-username",
        signup_token: "sk-oauthsu-abc",
        provider: "github",
      }),
  });
  const result = await completeOAuth(client, "github", {
    state: "s",
    code: "c",
  });
  assertEquals(result, {
    ok: true,
    kind: "needs-username",
    signupToken: "sk-oauthsu-abc",
    provider: "github",
  });
});

Deno.test("completeOAuth maps a 200 signin body", async () => {
  const client = asClient({
    oauthFinish: () =>
      Promise.resolve({ user_id: "u1", return_path: "/rooms" }),
  });
  const result = await completeOAuth(client, "github", {
    state: "s",
    code: "c",
  });
  assertEquals(result, {
    ok: true,
    kind: "signin",
    userId: "u1",
    returnPath: "/rooms",
  });
});

Deno.test("completeOAuthSignup maps a 409 duplicate username", async () => {
  const client = asClient({
    oauthSignupComplete: () =>
      Promise.reject(new ArcheionApiError(409, "username already taken")),
  });
  const result = await completeOAuthSignup(client, {
    signupToken: "sk-oauthsu-abc",
    username: "taken",
  });
  assertEquals(result, {
    ok: false,
    reason: "duplicate-username",
    message: "username already taken",
  });
});

Deno.test("completeOAuthSignup maps a 400 invalid username", async () => {
  const client = asClient({
    oauthSignupComplete: () =>
      Promise.reject(
        new ArcheionApiError(400, "username must be at least 3 chars"),
      ),
  });
  const result = await completeOAuthSignup(client, {
    signupToken: "sk-oauthsu-abc",
    username: "ab",
  });
  assertEquals(result, {
    ok: false,
    reason: "invalid-username",
    message: "username must be at least 3 chars",
  });
});

Deno.test("completeOAuthSignup maps a 403 signup-disabled", async () => {
  // The operator kill-switch (signup_enabled off) returns 403; surface the
  // distinct reason so the page shows specific copy rather than "unknown".
  const client = asClient({
    oauthSignupComplete: () =>
      Promise.reject(new ArcheionApiError(403, "signup disabled")),
  });
  const result = await completeOAuthSignup(client, {
    signupToken: "sk-oauthsu-abc",
    username: "someone",
  });
  assertEquals(result, {
    ok: false,
    reason: "signup-disabled",
    message: "signup disabled",
  });
});

Deno.test("adminLoginWithKey maps a 200 body to ok", async () => {
  const client = asClient({
    adminLogin: () => Promise.resolve({ user_id: "u1" }),
  });
  assertEquals(await adminLoginWithKey(client, "sk-admin-x"), {
    ok: true,
    userId: "u1",
  });
});

Deno.test("adminLoginWithKey maps 401/404 to typed statuses", async () => {
  const wrongKey = asClient({
    adminLogin: () =>
      Promise.reject(new ArcheionApiError(401, "admin token required")),
  });
  assertEquals(await adminLoginWithKey(wrongKey, "sk-admin-bad"), {
    ok: false,
    status: 401,
  });
  const unmounted = asClient({
    adminLogin: () => Promise.reject(new ArcheionApiError(404, "not found")),
  });
  assertEquals(await adminLoginWithKey(unmounted, "sk-admin-x"), {
    ok: false,
    status: 404,
  });
});

Deno.test("adminLoginWithKey rethrows transport-level errors", async () => {
  const client = asClient({
    adminLogin: () => Promise.reject(new Error("network down")),
  });
  await assertRejects(
    () => adminLoginWithKey(client, "sk-admin-x"),
    Error,
    "network down",
  );
});
