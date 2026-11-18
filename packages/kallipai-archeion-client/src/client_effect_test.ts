// The effect-level contract of the archeion verbs: each one succeeds with its
// route's value, a refusal arrives as `ArcheionApiError` carrying the envelope,
// and an exchange that produced no answer arrives as `TransportError`. The
// wire surface (path, method, CSRF marker, bearer, window) is pinned here too:
// the promise-shaped class inherits all of it unchanged.

import { withFetch } from "@kallipai/kallipai-common/testing";
import {
  assert,
  assertEquals,
  assertFalse,
  assertInstanceOf,
  assertRejects,
} from "@std/assert";
import { Cause, Effect, Exit, Result } from "effect";

import * as api from "./client.ts";
import { ArcheionClient } from "./client.ts";
import {
  addEmail,
  adminLogin,
  listExternalIdentities,
  loginBegin,
  logout,
  registerBegin,
  renamePasskey,
  resetTagmaCredential,
  usernameAvailability,
} from "./client.ts";
import { ArcheionApiError } from "./types.ts";

const BASE = "http://archeion.test/v1/archeion";

/** The typed failure of an exit, asserting it is not a defect. */
function failureOf<A, E>(exit: Exit.Exit<A, E>): E {
  assert(Exit.isFailure(exit), "expected a failure");
  const found = Cause.findError(exit.cause);
  assert(Result.isSuccess(found), "expected a typed failure, not a defect");
  return found.success;
}

/** The i-th element, asserting it exists: this tsconfig treats indexing as
 * possibly-undefined. */
function at<T>(xs: readonly T[], i: number): T {
  const value = xs[i];
  assert(value !== undefined, `expected an element at ${i}`);
  return value;
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status });

const answer = (response: Response) => () => Promise.resolve(response);

/** The headers of the one request a stub saw, as a plain record. */
function headersOf(init: RequestInit | undefined): Record<string, string> {
  return (init?.headers ?? {}) as Record<string, string>;
}

Deno.test("a verb succeeds with the value its route returns", async () => {
  await withFetch(
    answer(json({ ceremony_id: "c-1", options: {} })),
    async () => {
      const begun = await Effect.runPromise(
        registerBegin(BASE, { username: "ada" }),
      );
      assertEquals(begun.ceremony_id, "c-1");
    },
  );
});

Deno.test("the identities list projects GET /me", async () => {
  await withFetch(
    answer(
      json({
        external_identities: [{ id: "x-1", provider: "github" }],
      }),
    ),
    async () => {
      const identities = await Effect.runPromise(listExternalIdentities(BASE));
      assertEquals(identities.length, 1);
      assertEquals(at(identities, 0).id, "x-1");
    },
  );
});

Deno.test(
  "a refusal is an ArcheionApiError carrying the envelope",
  async () => {
    await withFetch(
      answer(
        json(
          { error: { message: "username already taken", code: "taken" } },
          409,
        ),
      ),
      async () => {
        const exit = await Effect.runPromiseExit(
          loginBegin(BASE, { username: "ada" }),
        );
        const error = failureOf(exit);
        assertInstanceOf(error, ArcheionApiError);
        assertEquals(error._tag, "ArcheionApiError");
        assertEquals(error.status, 409);
        assertEquals(error.message, "username already taken");
        assertEquals(error.code, "taken");
      },
    );
  },
);

Deno.test("a dead exchange is a TransportError keeping its cause", async () => {
  const boom = new TypeError("network down");
  await withFetch(
    () => Promise.reject(boom),
    async () => {
      const exit = await Effect.runPromiseExit(
        loginBegin(BASE, { username: "ada" }),
      );
      const error = failureOf(exit);
      assertEquals(error._tag, "TransportError");
      assertEquals(error.message, "network down");
      assertEquals(error.cause, boom);
    },
  );
});

Deno.test("an unreadable body is a TransportError, not a refusal", async () => {
  await withFetch(
    answer(new Response("<html>bad gateway</html>", { status: 200 })),
    async () => {
      const exit = await Effect.runPromiseExit(logout(BASE));
      const error = failureOf(exit);
      assertEquals(error._tag, "TransportError");
      assertEquals(error.message, "the response body could not be read");
    },
  );
});

Deno.test("a 204 and an empty body are successes with no value", async () => {
  await withFetch(answer(new Response(null, { status: 204 })), async () => {
    assertEquals(await Effect.runPromise(logout(BASE)), undefined);
  });
  await withFetch(
    answer(
      new Response(null, { status: 200, headers: { "content-length": "0" } }),
    ),
    async () => {
      assertEquals(await Effect.runPromise(logout(BASE)), undefined);
    },
  );
});

// `archeionRequest` turns an omitted window into `null`, so no verb can pick
// up the shared thirty-second default by leaving the field out. The signal
// that rides is the fiber's interruption, not a window.
Deno.test("every archeion call carries the fiber's interruption", async () => {
  const seen: (RequestInit | undefined)[] = [];
  await withFetch(
    (_input, init) => {
      seen.push(init);
      return Promise.resolve(json({}));
    },
    async () => {
      await Effect.runPromise(registerBegin(BASE, { username: "ada" }));
      await Effect.runPromise(addEmail(BASE, { address: "a@b.test" }));
      await Effect.runPromise(logout(BASE));
    },
  );
  assertEquals(seen.length, 3);
  for (const init of seen) {
    assert(init?.signal instanceof AbortSignal, "the fiber's signal must ride");
    assertFalse(init.signal.aborted, "no window of this plane is running");
  }
});

Deno.test(
  "the CSRF marker rides every state-changing method only",
  async () => {
    const seen: Record<string, string>[] = [];
    await withFetch(
      (_input, init) => {
        seen.push(headersOf(init));
        return Promise.resolve(json({}));
      },
      async () => {
        await Effect.runPromise(registerBegin(BASE, { username: "ada" }));
        await Effect.runPromise(renamePasskey(BASE, "pk-1", "laptop"));
        await Effect.runPromise(logout(BASE));
        await Effect.runPromise(usernameAvailability(BASE, "ada"));
      },
    );
    assertEquals(at(seen, 0)["X-Requested-With"], "kallipai");
    assertEquals(at(seen, 1)["X-Requested-With"], "kallipai");
    assertEquals(at(seen, 2)["X-Requested-With"], "kallipai");
    assert(
      !("X-Requested-With" in at(seen, 3)),
      "GET must not carry the CSRF marker",
    );
  },
);

Deno.test("the admin exchange carries the bearer and no body", async () => {
  const seen: RequestInit[] = [];
  await withFetch(
    (_input, init) => {
      seen.push(init ?? {});
      return Promise.resolve(json({ user_id: "u-1" }));
    },
    async () => {
      await Effect.runPromise(adminLogin(BASE, "sk-admin-key"));
      await Effect.runPromise(registerBegin(BASE, { username: "ada" }));
    },
  );
  assertEquals(headersOf(seen[0]).authorization, "Bearer sk-admin-key");
  assertEquals(at(seen, 0).body, undefined);
  assertEquals(headersOf(seen[1]).authorization, undefined);
});

Deno.test(
  "a JSON body brings its content-type; a bare POST does not",
  async () => {
    const seen: Record<string, string>[] = [];
    await withFetch(
      (_input, init) => {
        seen.push(headersOf(init));
        return Promise.resolve(json({}));
      },
      async () => {
        await Effect.runPromise(addEmail(BASE, { address: "a@b.test" }));
        await Effect.runPromise(logout(BASE));
      },
    );
    assertEquals(at(seen, 0)["content-type"], "application/json");
    assertEquals(at(seen, 0).accept, "application/json");
    assertEquals(at(seen, 1)["content-type"], undefined);
  },
);

Deno.test("the verbs carry their route and method verbatim", async () => {
  const seen: { url: string; method: string | undefined }[] = [];
  await withFetch(
    (input, init) => {
      seen.push({ url: String(input), method: init?.method });
      return Promise.resolve(json({}));
    },
    async () => {
      await Effect.runPromise(usernameAvailability(BASE, "ada lovelace"));
      await Effect.runPromise(renamePasskey(BASE, "pk-1", "laptop"));
      await Effect.runPromise(resetTagmaCredential(BASE, "t-1"));
    },
  );
  assertEquals(
    at(seen, 0).url,
    `${BASE}/auth/username-availability?username=ada%20lovelace`,
  );
  assertEquals(at(seen, 0).method, "GET");
  assertEquals(at(seen, 1).url, `${BASE}/me/passkeys/pk-1`);
  assertEquals(at(seen, 1).method, "PATCH");
  assertEquals(at(seen, 2).url, `${BASE}/tagmata/t-1/credential/reset`);
  assertEquals(at(seen, 2).method, "POST");
});

// Every verb's route and method, in one table: the 44 function bodies are
// identical by construction, so a mistyped path is invisible in review and
// only a table like this one catches it. One row per verb, plus
// `addPasskeyBegin`'s discoverable variant; a verb added without a row is
// caught by the set comparison at the end of this file, which checks only
// that no verb is missing (urls are the earlier tests' business).
type WireCase = {
  readonly name: string;
  readonly url: string;
  readonly method: string;
  readonly args: readonly unknown[];
};

const nothing = {} as never;
const WIRE: readonly WireCase[] = [
  {
    name: "registerBegin",
    url: "/auth/register/begin",
    method: "POST",
    args: [{ username: "u" }],
  },
  {
    name: "registerFinish",
    url: "/auth/register/finish",
    method: "POST",
    args: [{ ceremony_id: "c", credential: nothing }],
  },
  {
    name: "loginBegin",
    url: "/auth/login/begin",
    method: "POST",
    args: [{ username: "u" }],
  },
  {
    name: "loginFinish",
    url: "/auth/login/finish",
    method: "POST",
    args: [{ ceremony_id: "c", credential: nothing }],
  },
  { name: "adminLogin", url: "/auth/admin-login", method: "POST", args: ["k"] },
  {
    name: "usernameAvailability",
    url: "/auth/username-availability?username=u",
    method: "GET",
    args: ["u"],
  },
  {
    name: "loginDiscoverableBegin",
    url: "/auth/login/discoverable/begin",
    method: "POST",
    args: [],
  },
  {
    name: "loginDiscoverableFinish",
    url: "/auth/login/discoverable/finish",
    method: "POST",
    args: [{ ceremony_id: "c", credential: nothing }],
  },
  { name: "logout", url: "/auth/logout", method: "POST", args: [] },
  {
    name: "listOAuthProviders",
    url: "/auth/oauth/providers",
    method: "GET",
    args: [],
  },
  {
    name: "oauthSignInBegin",
    url: "/auth/oauth/github/begin",
    method: "POST",
    args: ["github", {}],
  },
  {
    name: "oauthLinkBegin",
    url: "/me/oauth/github/begin",
    method: "POST",
    args: ["github", {}],
  },
  {
    name: "oauthFinish",
    url: "/auth/oauth/github/finish",
    method: "POST",
    args: ["github", nothing],
  },
  {
    name: "oauthSignupComplete",
    url: "/auth/oauth/signup/complete",
    method: "POST",
    args: [nothing],
  },
  { name: "listExternalIdentities", url: "/me", method: "GET", args: [] },
  {
    name: "unlinkExternalIdentity",
    url: "/me/external-identities/x",
    method: "DELETE",
    args: ["x"],
  },
  { name: "me", url: "/me", method: "GET", args: [] },
  { name: "listEmails", url: "/me/emails", method: "GET", args: [] },
  {
    name: "addEmail",
    url: "/me/emails",
    method: "POST",
    args: [{ address: "a@b.test" }],
  },
  {
    name: "verifyEmail",
    url: "/me/emails/verify",
    method: "POST",
    args: [{ token: "t" }],
  },
  {
    name: "makeEmailPrimary",
    url: "/me/emails/e1",
    method: "POST",
    args: ["e1"],
  },
  { name: "removeEmail", url: "/me/emails/e1", method: "DELETE", args: ["e1"] },
  { name: "listPasskeys", url: "/me/passkeys", method: "GET", args: [] },
  {
    name: "addPasskeyBegin",
    url: "/me/passkeys/register/begin",
    method: "POST",
    args: [],
  },
  {
    name: "addPasskeyBegin",
    url: "/me/passkeys/register/begin?discoverable=true",
    method: "POST",
    args: [{ discoverable: true }],
  },
  {
    name: "addPasskeyFinish",
    url: "/me/passkeys/register/finish",
    method: "POST",
    args: [nothing],
  },
  {
    name: "renamePasskey",
    url: "/me/passkeys/p1",
    method: "PATCH",
    args: ["p1", "l"],
  },
  {
    name: "revokePasskey",
    url: "/me/passkeys/p1",
    method: "DELETE",
    args: ["p1"],
  },
  {
    name: "mintPairingCode",
    url: "/me/device-pairing",
    method: "POST",
    args: [],
  },
  {
    name: "pairBegin",
    url: "/auth/device-pairing/begin",
    method: "POST",
    args: [nothing],
  },
  {
    name: "pairFinish",
    url: "/auth/device-pairing/finish",
    method: "POST",
    args: [nothing],
  },
  { name: "mintTagma", url: "/tagmata", method: "POST", args: [] },
  { name: "listTagmata", url: "/tagmata", method: "GET", args: [] },
  {
    name: "renameTagma",
    url: "/tagmata/t1",
    method: "PATCH",
    args: ["t1", null],
  },
  { name: "revokeTagma", url: "/tagmata/t1", method: "DELETE", args: ["t1"] },
  { name: "getTagma", url: "/tagmata/t1", method: "GET", args: ["t1"] },
  { name: "getUserProfile", url: "/users/ada", method: "GET", args: ["ada"] },
  {
    name: "getTagmaProfile",
    url: "/tagmata/t1/profile",
    method: "GET",
    args: ["t1"],
  },
  { name: "listTokens", url: "/me/tokens", method: "GET", args: [] },
  { name: "createToken", url: "/me/tokens", method: "POST", args: [nothing] },
  {
    name: "updateToken",
    url: "/me/tokens/k1",
    method: "PATCH",
    args: ["k1", {}],
  },
  { name: "revokeToken", url: "/me/tokens/k1", method: "DELETE", args: ["k1"] },
  {
    name: "getTagmaCredential",
    url: "/tagmata/t1/credential",
    method: "GET",
    args: ["t1"],
  },
  {
    name: "updateTagmaCredential",
    url: "/tagmata/t1/credential",
    method: "PATCH",
    args: ["t1", {}],
  },
  {
    name: "resetTagmaCredential",
    url: "/tagmata/t1/credential/reset",
    method: "POST",
    args: ["t1"],
  },
];

/** The verb functions, addressed by the name the table records. */
const VERBS = api as unknown as Record<
  string,
  | ((base: string, ...args: unknown[]) => Effect.Effect<unknown, unknown>)
  | undefined
>;

Deno.test("every verb sends its route and method", async () => {
  const seen: { url: string; method: string | undefined }[] = [];
  await withFetch(
    (input, init) => {
      seen.push({ url: String(input), method: init?.method });
      return Promise.resolve(json({}));
    },
    async () => {
      for (const wire of WIRE) {
        const verb = VERBS[wire.name];
        assert(verb !== undefined, `no verb named ${wire.name}`);
        await Effect.runPromise(verb(BASE, ...wire.args));
      }
    },
  );
  assertEquals(seen.length, WIRE.length);
  WIRE.forEach((wire, i) => {
    assertEquals(at(seen, i).url, BASE + wire.url);
    assertEquals(at(seen, i).method, wire.method);
  });
});

Deno.test("every class method reaches its verb's route", async () => {
  const client = new ArcheionClient(BASE) as unknown as Record<
    string,
    ((...args: unknown[]) => Promise<unknown>) | undefined
  >;
  const seen: { url: string; method: string | undefined }[] = [];
  await withFetch(
    (input, init) => {
      seen.push({ url: String(input), method: init?.method });
      return Promise.resolve(json({}));
    },
    async () => {
      for (const wire of WIRE) {
        const method = client[wire.name];
        assert(method !== undefined, `no method named ${wire.name}`);
        await method.apply(client, [...wire.args]);
      }
    },
  );
  assertEquals(seen.length, WIRE.length);
  WIRE.forEach((wire, i) => {
    assertEquals(at(seen, i).url, BASE + wire.url);
    assertEquals(at(seen, i).method, wire.method);
  });
});

Deno.test("the promise face rejects with the failure value", async () => {
  await withFetch(
    answer(json({ error: { message: "no session" } }, 401)),
    async () => {
      await assertRejects(
        () => new ArcheionClient(BASE).loginBegin({ username: "u" }),
        ArcheionApiError,
        "no session",
      );
    },
  );
});

Deno.test("a trailing slash on the base is trimmed", async () => {
  const seen: string[] = [];
  await withFetch(
    (input) => {
      seen.push(String(input));
      return Promise.resolve(json({}));
    },
    async () => {
      await new ArcheionClient(`${BASE}/`).me();
    },
  );
  // Untrimmed, the concatenation would put a double slash before the path.
  assertEquals(at(seen, 0), `${BASE}/me`);
});

Deno.test("the table has a row for every verb", () => {
  // A shape net, not a correctness net: it cannot tell whether a url is
  // right, only whether a verb exists that no row exercises. Without it, a
  // new verb would leave the table silently incomplete and everything green.
  const exported = Object.entries(api)
    .filter(
      ([name, value]) =>
        name !== "ArcheionClient" && typeof value === "function",
    )
    .map(([name]) => name)
    .sort();
  const tabled = [...new Set(WIRE.map((wire) => wire.name))].sort();
  assertEquals(tabled, exported);
});
