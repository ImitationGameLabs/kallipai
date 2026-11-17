// jsonRequest: the shared fetch discipline as an Effect. The status mapping,
// the CSRF marker, the timeout window, the 204/empty-body shape, and the data
// failure channel -- a refused connection or an unreadable body arrives as a
// transport failure, a non-2xx as an api failure. dataPlaneRequest maps those
// onto the data plane's error classes; the last test pins the transitional
// seam, where a rejected BaseClient.json call hands the caller the error
// instance itself.

import { assert, assertEquals, assertFalse } from "@std/assert";
import { Cause, Effect, Exit, Result } from "effect";
import {
  BaseClient,
  CSRF_HEADER,
  dataPlaneRequest,
  jsonRequest,
} from "./http.ts";
import { DataPlaneApiError, TransportError } from "./errors.ts";
import { withFetch } from "./testing.ts";

/** The typed failure of an exit, asserting it is not a defect. */
function failureOf<A, E>(exit: Exit.Exit<A, E>): E {
  assert(Exit.isFailure(exit), "expected a failure");
  const found = Cause.findError(exit.cause);
  assert(Result.isSuccess(found), "expected a typed failure, not a defect");
  return found.success;
}

const jsonResponse = (body: string) => new Response(body);

Deno.test("jsonRequest returns the parsed body", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        jsonResponse(JSON.stringify({ ok: true })),
      )) as typeof fetch,
    async () => {
      const value = await Effect.runPromise(
        jsonRequest<{ ok: boolean }>({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      assertEquals(value, { ok: true });
    },
  );
});

Deno.test("jsonRequest sends the CSRF marker on a non-GET", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        jsonRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "POST",
          body: { a: 1 },
        }),
      );
      const headers = sent?.headers as Record<string, string>;
      assertEquals(headers[CSRF_HEADER], "kallipai");
      assertEquals(headers["content-type"], "application/json");
      assertEquals(sent?.credentials, "include");
      assertEquals(sent?.body, JSON.stringify({ a: 1 }));
    },
  );
});

Deno.test("jsonRequest leaves a GET free of the mutating headers", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
      );
      const headers = sent?.headers as Record<string, string>;
      assertEquals(headers.accept, "application/json");
      assertFalse(CSRF_HEADER in headers, "GET must not carry the CSRF marker");
      assertFalse("content-type" in headers, "GET sends no body");
      assertEquals(sent?.body, undefined);
    },
  );
});

Deno.test("jsonRequest merges caller headers", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        jsonRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
          headers: { authorization: "Bearer t0ken" },
        }),
      );
      const headers = sent?.headers as Record<string, string>;
      assertEquals(headers.authorization, "Bearer t0ken");
      assertEquals(headers.accept, "application/json");
    },
  );
});

Deno.test("jsonRequest bounds the exchange by default", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
      );
      assert(
        sent?.signal instanceof AbortSignal,
        "the default window must pass a signal",
      );
      assertFalse(sent.signal.aborted, "a fresh window is not yet aborted");
    },
  );
});

Deno.test("jsonRequest sends no signal when the window is null", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        jsonRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
          timeoutMs: null,
        }),
      );
      assertFalse("signal" in (sent as RequestInit), "null means no window");
    },
  );
});

Deno.test("a zero or negative window aborts before the request", async () => {
  const sent: (RequestInit | undefined)[] = [];
  // A real fetch refuses an already-aborted signal; the stub does the same so
  // the failure mapping is exercised rather than bypassed.
  const aborting = (_input: string | URL, init?: RequestInit) => {
    sent.push(init);
    return init?.signal?.aborted
      ? Promise.reject(
          new DOMException("The operation was aborted.", "AbortError"),
        )
      : Promise.resolve(jsonResponse("{}"));
  };
  await withFetch(aborting, async () => {
    for (const timeoutMs of [0, -1]) {
      const exit = await Effect.runPromiseExit(
        jsonRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
          timeoutMs,
        }),
      );
      const failure = failureOf(exit);
      assertEquals(failure.kind, "transport");
    }
  });
  assertEquals(sent[0]?.signal?.aborted, true);
  assertEquals(sent[1]?.signal?.aborted, true);
});

Deno.test("the CSRF marker rides state-changing methods only", async () => {
  const sent: (RequestInit | undefined)[] = [];
  await withFetch(
    (_input, init) => {
      sent.push(init);
      return Promise.resolve(jsonResponse("{}"));
    },
    async () => {
      for (const method of [
        "GET",
        "HEAD",
        "OPTIONS",
        "PUT",
        "POST",
        "DELETE",
      ]) {
        await Effect.runPromise(
          jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method }),
        );
      }
    },
  );
  const marker = sent.map(
    (init) => ((init?.headers ?? {}) as Record<string, string>)[CSRF_HEADER],
  );
  assertEquals(marker[0], undefined);
  assertEquals(marker[1], undefined);
  assertEquals(marker[2], undefined);
  assertEquals(marker[3], "kallipai");
  assertEquals(marker[4], "kallipai");
  assertEquals(marker[5], "kallipai");
});

Deno.test("jsonRequest treats 204 and an empty body as undefined", async () => {
  for (const resp of [
    new Response(null, { status: 204 }),
    new Response("", { status: 200, headers: { "content-length": "0" } }),
  ]) {
    await withFetch((() => Promise.resolve(resp)) as typeof fetch, async () => {
      const value = await Effect.runPromise(
        jsonRequest<undefined>({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "DELETE",
        }),
      );
      assertEquals(value, undefined);
    });
  }
});

Deno.test("jsonRequest carries a non-2xx envelope as api data", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            error: { message: "gone", code: "missing" },
          }),
          { status: 404, statusText: "Not Found" },
        ),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
      );
      const failure = failureOf(exit);
      assertEquals(failure, {
        kind: "api",
        status: 404,
        message: "gone",
        code: "missing",
      });
    },
  );
});

Deno.test(
  "jsonRequest maps a refused connection to transport data",
  async () => {
    const cause = new TypeError("connection refused");
    await withFetch((() => Promise.reject(cause)) as typeof fetch, async () => {
      const exit = await Effect.runPromiseExit(
        jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
      );
      const failure = failureOf(exit);
      assert(
        failure.kind === "transport",
        `expected transport, got ${failure.kind}`,
      );
      // The message is the reason verbatim: user-facing call sites render it,
      // and a route in there leaks an internal path.
      assertEquals(failure.message, "connection refused");
      assertEquals(failure.cause, cause);
    });
  },
);

Deno.test("jsonRequest maps a closed window to transport data", async () => {
  const cause = new DOMException("The operation timed out.", "TimeoutError");
  await withFetch((() => Promise.reject(cause)) as typeof fetch, async () => {
    const exit = await Effect.runPromiseExit(
      jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
    );
    const failure = failureOf(exit);
    assert(
      failure.kind === "transport",
      `expected transport, got ${failure.kind}`,
    );
    assertEquals(failure.message, "The operation timed out.");
    assertEquals(failure.cause, cause);
  });
});

Deno.test("jsonRequest maps an unreadable body to transport data", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response("not json at all", {
          status: 200,
          headers: { "content-length": "15" },
        }),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        jsonRequest({ baseUrl: "http://relay", path: "/v1/x", method: "GET" }),
      );
      const failure = failureOf(exit);
      assert(
        failure.kind === "transport",
        `expected transport, got ${failure.kind}`,
      );
      assertEquals(failure.message, "the response body could not be read");
      assert(
        failure.cause instanceof SyntaxError,
        `expected the parse error as cause, got ${String(failure.cause)}`,
      );
    },
  );
});

Deno.test("dataPlaneRequest maps api data to DataPlaneApiError", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(JSON.stringify({ error: { message: "gone" } }), {
          status: 404,
          statusText: "Not Found",
        }),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        dataPlaneRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      const error = failureOf(exit);
      assert(error instanceof DataPlaneApiError, "expected DataPlaneApiError");
      assertEquals(error._tag, "DataPlaneApiError");
      assertEquals(error.status, 404);
      assertEquals(error.message, "gone");
    },
  );
});

Deno.test(
  "dataPlaneRequest maps transport data to TransportError",
  async () => {
    const cause = new TypeError("connection refused");
    await withFetch((() => Promise.reject(cause)) as typeof fetch, async () => {
      const exit = await Effect.runPromiseExit(
        dataPlaneRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      const error = failureOf(exit);
      assert(error instanceof TransportError, "expected TransportError");
      assertEquals(error.message, "connection refused");
      assertEquals(error.cause, cause);
      assertFalse(
        error.message.includes("/v1/x"),
        "the transport message must not carry the route",
      );
    });
  },
);

class ProbeClient extends BaseClient {
  read<T>(path: string): Promise<T> {
    return this.json<T>(path, "GET");
  }
}

Deno.test(
  "BaseClient.json rejects with the error instance, not a wrapper",
  async () => {
    await withFetch(
      (() =>
        Promise.resolve(
          new Response(JSON.stringify({ error: { message: "nope" } }), {
            status: 404,
            statusText: "Not Found",
          }),
        )) as typeof fetch,
      async () => {
        let caught: unknown;
        try {
          await new ProbeClient("http://relay").read("/v1/x");
        } catch (error) {
          caught = error;
        }
        assert(
          caught instanceof DataPlaneApiError,
          `expected the error instance, got ${String(caught)}`,
        );
        assertEquals((caught as DataPlaneApiError)._tag, "DataPlaneApiError");
        assertEquals((caught as DataPlaneApiError).status, 404);
      },
    );
  },
);
