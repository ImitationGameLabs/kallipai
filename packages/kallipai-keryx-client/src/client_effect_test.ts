// The failure channel of the keryx verbs: a refusal and a dead exchange
// arrive as values, the POST verbs carry the CSRF marker, and the stream
// verb carries the same two. The wire shape is pinned by `events.test.ts`
// and `keryx-route-shapes_test.ts`; these legs pin the typed failures a
// caller sees when it runs the verb's effect.

import { CSRF_HEADER, TransportError } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { assert, assertEquals } from "@std/assert";
import { Cause, Effect, Exit, Result, Stream } from "effect";

import { createConversation, keryxEvents, postEnvelope } from "./client.ts";
import type { Envelope } from "./types.ts";
import { DataPlaneApiError } from "./types.ts";

const BASE = "http://x";

/** The typed failure of an exit, asserting it is not a defect. */
function failureOf<A, E>(exit: Exit.Exit<A, E>): E {
  assert(Exit.isFailure(exit), "expected a failure");
  const found = Cause.findError(exit.cause);
  assert(Result.isSuccess(found), "expected a typed failure, not a defect");
  return found.success;
}

Deno.test("a verb's refusal is a DataPlaneApiError value", async () => {
  let seen: string | undefined;
  await withFetch(
    ((input) => {
      seen = String(input);
      return Promise.resolve(
        Response.json(
          { error: { message: "tagma offline", code: "tagma_offline" } },
          { status: 503 },
        ),
      );
    }) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(createConversation(BASE, "t-a"));
      const failure = failureOf(exit);
      assert(failure instanceof DataPlaneApiError);
      assertEquals(failure.status, 503);
      assertEquals(failure.message, "tagma offline");
      assertEquals(failure.code, "tagma_offline");
      assertEquals(seen, `${BASE}/conversations`);
    },
  );
});

Deno.test("a verb's dead exchange is a TransportError value", async () => {
  await withFetch(
    (() => Promise.reject(new TypeError("fetch failed"))) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(createConversation(BASE, "t-a"));
      assert(failureOf(exit) instanceof TransportError);
    },
  );
});

Deno.test("a state-changing verb carries the CSRF marker", async () => {
  let headers: Record<string, string> = {};
  await withFetch(
    ((_input: string | URL, init?: RequestInit) => {
      headers = (init?.headers ?? {}) as Record<string, string>;
      return Promise.resolve(Response.json({ conversation_id: "c1" }));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(createConversation(BASE, "t-a"));
      assertEquals(headers[CSRF_HEADER], "kallipai");
      assertEquals(headers["content-type"], "application/json");
    },
  );
});

Deno.test("the stream verb carries a refusal as a value", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        Response.json({ error: { message: "unauthorized" } }, { status: 401 }),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        Stream.runCollect(keryxEvents(BASE)),
      );
      const failure = failureOf(exit);
      assert(failure instanceof DataPlaneApiError);
      assertEquals(failure.status, 401);
    },
  );
});

Deno.test("a bodyless 202 accepts the envelope", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(null, {
          status: 202,
          headers: { "content-length": "0" },
        }),
      )) as typeof fetch,
    async () => {
      // `POST /conversations/{id}/envelopes` answers 202 with no body; the
      // JSON seam's empty-body short circuit is what makes that `undefined`.
      const accepted = await Effect.runPromise(
        postEnvelope(BASE, "c1", {} as Envelope),
      );
      assertEquals(accepted, undefined);
    },
  );
});
