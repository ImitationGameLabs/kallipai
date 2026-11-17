// The effect-level contract of the files verbs: each one succeeds with its
// value, a refusal arrives as `FilesApiError` carrying the envelope, and an
// exchange that produced no answer arrives as `TransportError`. The
// promise-shaped class is pinned separately in `client_test.ts`.

import { TransportError } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { assert, assertEquals, assertInstanceOf } from "@std/assert";
import { Cause, Effect, Exit, Result } from "effect";

import {
  deleteRecord,
  getRecord,
  headRecord,
  listRecords,
  putRecord,
  sendRecord,
} from "./client.ts";
import { FilesApiError, filesFetch } from "./http.ts";

const BASE = "http://files.test/v1/files";

/** The typed failure of an exit, asserting it is not a defect. */
function failureOf<A, E>(exit: Exit.Exit<A, E>): E {
  assert(Exit.isFailure(exit), "expected a failure");
  const found = Cause.findError(exit.cause);
  assert(Result.isSuccess(found), "expected a typed failure, not a defect");
  return found.success;
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status });

const answer = (response: Response) => () => Promise.resolve(response);

Deno.test("each verb succeeds with the value its route returns", async () => {
  await withFetch(
    answer(json({ record_id: "r-1", blob_id: "b-1" })),
    async () => {
      const out = await Effect.runPromise(
        putRecord(BASE, "/users/u1/shared/a.pdf", new Uint8Array([1])),
      );
      assertEquals(out.blob_id, "b-1");
    },
  );
});

Deno.test("the download hands back the bytes", async () => {
  await withFetch(answer(new Response(new Uint8Array([4, 5, 6]))), async () => {
    const bytes = await Effect.runPromise(getRecord(BASE, "r-1"));
    assertEquals(new Uint8Array(bytes), new Uint8Array([4, 5, 6]));
  });
});

Deno.test("the listing maps the entries array", async () => {
  await withFetch(
    answer(json([{ id: "r-1", path: "/p", size: 2, created_at: "2026-01" }])),
    async () => {
      const entries = await Effect.runPromise(
        listRecords(BASE, { space: "self" }),
      );
      assertEquals(entries.length, 1);
      assertEquals(entries[0].id, "r-1");
    },
  );
});

Deno.test("delivery maps the recipient's copy", async () => {
  await withFetch(
    answer(json({ record_id: "r-2", blob_id: "b-1", path: "/inbox/a.pdf" })),
    async () => {
      const sent = await Effect.runPromise(
        sendRecord(BASE, "r-1", { toUser: "u2" }),
      );
      assertEquals(sent.record_id, "r-2");
    },
  );
});

Deno.test("deletion succeeds with no value on a 204", async () => {
  await withFetch(answer(new Response(null, { status: 204 })), async () => {
    const value = await Effect.runPromise(deleteRecord(BASE, "r-1"));
    assertEquals(value, undefined);
  });
});

Deno.test("no files call carries a timeout window", async () => {
  const seen: (RequestInit | undefined)[] = [];
  await withFetch(
    (_input, init) => {
      seen.push(init);
      return Promise.resolve(json({ record_id: "r-1", blob_id: "b-1" }));
    },
    async () => {
      await Effect.runPromise(putRecord(BASE, "/p", new Uint8Array([1])));
      await Effect.runPromise(getRecord(BASE, "r-1"));
      await Effect.runPromise(headRecord(BASE, "r-1"));
      await Effect.runPromise(listRecords(BASE, { space: "self" }));
      await Effect.runPromise(deleteRecord(BASE, "r-1"));
      await Effect.runPromise(sendRecord(BASE, "r-1", { toUser: "u2" }));
    },
  );
  assertEquals(seen.length, 6);
  for (const init of seen) assertEquals(init?.signal, undefined);
});

Deno.test("a refusal is a FilesApiError carrying the envelope", async () => {
  await withFetch(
    answer(
      json({ error: { message: "no read right", code: "no_read_right" } }, 403),
    ),
    async () => {
      const exit = await Effect.runPromiseExit(getRecord(BASE, "r-1"));
      const error = failureOf(exit);
      assertInstanceOf(error, FilesApiError);
      assertEquals(error._tag, "FilesApiError");
      assertEquals(error.status, 403);
      assertEquals(error.message, "no read right");
      assertEquals(error.code, "no_read_right");
    },
  );
});

Deno.test("a dead exchange is a TransportError keeping its cause", async () => {
  const boom = new TypeError("network down");
  await withFetch(
    () => Promise.reject(boom),
    async () => {
      const exit = await Effect.runPromiseExit(
        listRecords(BASE, { space: "self" }),
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
      const exit = await Effect.runPromiseExit(
        listRecords(BASE, { space: "self" }),
      );
      const error = failureOf(exit);
      assertEquals(error._tag, "TransportError");
      assertEquals(error.message, "the response body could not be read");
    },
  );
});

Deno.test("a window of zero or less aborts as a dead exchange", async () => {
  const aborting = (_input: string | URL, init?: RequestInit) =>
    init?.signal?.aborted
      ? Promise.reject(
          new DOMException("The operation was aborted.", "AbortError"),
        )
      : Promise.resolve(json({}));
  await withFetch(aborting, async () => {
    for (const timeoutMs of [0, -1]) {
      const exit = await Effect.runPromiseExit(
        filesFetch(BASE, "/r-1", { method: "GET", timeoutMs }),
      );
      const error = failureOf(exit);
      assertEquals(error._tag, "TransportError");
    }
  });
});

Deno.test(
  "head answers existence: 404 is false, other refusals pass",
  async () => {
    await withFetch(answer(new Response(null)), async () => {
      assertEquals(await Effect.runPromise(headRecord(BASE, "r-1")), true);
    });

    await withFetch(
      // A HEAD answer carries no body, so this is what the wire sends.
      answer(new Response(null, { status: 404 })),
      async () => {
        assertEquals(await Effect.runPromise(headRecord(BASE, "r-1")), false);
      },
    );

    for (const status of [403, 500]) {
      await withFetch(
        // Same shape: status line only, no envelope to read.
        answer(new Response(null, { status })),
        async () => {
          const failure = failureOf(
            await Effect.runPromiseExit(headRecord(BASE, "r-1")),
          );
          assertInstanceOf(failure, FilesApiError);
          assertEquals(failure.status, status);
          // The message is empty here: the mock carries no reason phrase,
          // and a HEAD answer has no body to carry an envelope.
        },
      );
    }

    await withFetch(
      () => Promise.reject(new TypeError("reset")),
      async () => {
        const failure = failureOf(
          await Effect.runPromiseExit(headRecord(BASE, "r-1")),
        );
        assertInstanceOf(failure, TransportError);
      },
    );
  },
);
