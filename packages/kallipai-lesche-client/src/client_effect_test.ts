// The effect face of the lesche verbs: a refusal and a dead exchange arrive
// in the failure channel as values, and the stream verb carries the same
// two. The promise-shaped class is pinned by `rooms.test.ts` and
// `mevents.test.ts`; these legs pin the surface a consumer reaches for when
// it stops awaiting.

import { TransportError } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { assert, assertEquals } from "@std/assert";
import { Cause, Effect, Exit, Result, Stream } from "effect";

import { listRooms, roomEvents } from "./client.ts";
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
          { error: { message: "not a member", code: "not_member" } },
          { status: 403 },
        ),
      );
    }) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(listRooms(BASE));
      const failure = failureOf(exit);
      assert(failure instanceof DataPlaneApiError);
      assertEquals(failure.status, 403);
      assertEquals(failure.message, "not a member");
      assertEquals(failure.code, "not_member");
      assertEquals(seen, `${BASE}/rooms`);
    },
  );
});

Deno.test("a verb's dead exchange is a TransportError value", async () => {
  await withFetch(
    (() => Promise.reject(new TypeError("fetch failed"))) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(listRooms(BASE));
      assert(failureOf(exit) instanceof TransportError);
    },
  );
});

Deno.test("the stream verb carries a refusal as a value", async () => {
  let seen: string | undefined;
  await withFetch(
    ((input) => {
      seen = String(input);
      return Promise.resolve(
        Response.json(
          { error: { message: "session expired" } },
          {
            status: 401,
          },
        ),
      );
    }) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        Stream.runCollect(roomEvents(BASE)),
      );
      const failure = failureOf(exit);
      assert(failure instanceof DataPlaneApiError);
      assertEquals(failure.status, 401);
      assertEquals(seen, `${BASE}/me/events`);
    },
  );
});
