// jsonRequest: the shared fetch discipline as an Effect. The status mapping,
// the CSRF marker, the timeout window, the 204/empty-body shape, and the data
// failure channel -- a refused connection or an unreadable body arrives as a
// transport failure, a non-2xx as an api failure. dataPlaneRequest maps those
// onto the data plane's error classes; the last test pins the Promise boundary,
// where a rejected dataPlaneRequest hands the caller the error instance itself.

import { assert, assertEquals, assertFalse } from "@std/assert";
import { Cause, Effect, Exit, Fiber, Result } from "effect";
import {
  CSRF_HEADER,
  dataPlaneRequest,
  fetchRequest,
  JSON_REQUEST_TIMEOUT_MS,
  jsonRequest,
  runClientEffect,
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
          credentials: "include",
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
          credentials: "include",
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
        jsonRequest({
          credentials: "include",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      const headers = sent?.headers as Record<string, string>;
      assertEquals(headers.accept, "application/json");
      assertFalse(CSRF_HEADER in headers, "GET must not carry the CSRF marker");
      assertFalse("content-type" in headers, "GET sends no body");
      assertEquals(sent?.body, undefined);
    },
  );
});

Deno.test(
  "jsonRequest hands the caller's credential mode to fetch",
  async () => {
    let sent: RequestInit | undefined;
    await withFetch(
      ((_url: string, init?: RequestInit) => {
        sent = init;
        return Promise.resolve(jsonResponse("{}"));
      }) as typeof fetch,
      async () => {
        await Effect.runPromise(
          jsonRequest({
            credentials: "omit",
            baseUrl: "http://relay",
            path: "/v1/x",
            method: "GET",
          }),
        );
        assertEquals(sent?.credentials, "omit");
      },
    );
  },
);

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
          credentials: "include",
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

Deno.test(
  "the window factory sees null, an omitted window and a value",
  async (t) => {
    // A signal that has not aborted yet cannot tell "no window" from "a window
    // nobody waited out", so the three states are pinned where they are made:
    // on the factory that turns a window into a signal.
    const realTimeout = AbortSignal.timeout;
    const calls: number[] = [];
    AbortSignal.timeout = ((ms: number) => {
      calls.push(ms);
      return realTimeout.call(AbortSignal, ms);
    }) as typeof AbortSignal.timeout;
    /** A fetch that never answers on its own: only its signal ends it. */
    let fetches = 0;
    /** The reason the signal carried when `fetch` saw the abort. */
    let abortReason: unknown;
    const stalled = ((_input: string | URL, init?: RequestInit) => {
      fetches += 1;
      return new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener("abort", () => {
          abortReason = init?.signal?.reason;
          reject(new DOMException("The operation was aborted.", "AbortError"));
        });
      });
    }) as typeof fetch;
    /**
     * Start a request, give it a moment to reach `fetch`, then abandon it.
     * Returns how many times `fetch` was dialled: an empty `calls` proves no
     * window only if the request got as far as asking for one.
     */
    const abandon = async (effect: Effect.Effect<unknown, unknown>) => {
      const before = fetches;
      const fiber = Effect.runFork(effect);
      await new Promise((resolve) => setTimeout(resolve, 50));
      await Effect.runPromise(Fiber.interrupt(fiber));
      return fetches - before;
    };
    try {
      await withFetch(stalled, async () => {
        await t.step("an explicit null", async () => {
          const dialled = await abandon(
            fetchRequest({
              baseUrl: "http://relay",
              path: "/v1/x",
              method: "GET",
              timeoutMs: null,
              credentials: "include",
            }),
          );
          assertEquals(dialled, 1, "the request must reach fetch");
          assertEquals(calls, [], "an explicit null must not start a window");
        });

        await t.step("a null through the JSON seam", async () => {
          // A null that survives the seam is what keeps a plane asking for no
          // window from inheriting the default.
          const dialled = await abandon(
            jsonRequest({
              credentials: "include",
              baseUrl: "http://relay",
              path: "/v1/x",
              method: "GET",
              timeoutMs: null,
            }),
          );
          assertEquals(dialled, 1, "the request must reach fetch");
          assertEquals(calls, [], "a null window at the seam stays no window");
        });

        await t.step("an omitted window", async () => {
          const dialled = await abandon(
            jsonRequest({
              credentials: "include",
              baseUrl: "http://relay",
              path: "/v1/x",
              method: "GET",
            }),
          );
          assertEquals(dialled, 1, "the request must reach fetch");
          assertEquals(
            calls,
            [JSON_REQUEST_TIMEOUT_MS],
            "an omitted window takes the shared default",
          );
          calls.length = 0;
        });

        await t.step("a value", async () => {
          const started = performance.now();
          // A window that never fires would leave this exchange pending, and a
          // pending test reads as a hang; the race turns that into a failure.
          const exit = await Promise.race([
            Effect.runPromiseExit(
              jsonRequest({
                credentials: "include",
                baseUrl: "http://relay",
                path: "/v1/x",
                method: "GET",
                timeoutMs: 10,
              }),
            ),
            new Promise<never>((_resolve, reject) =>
              setTimeout(
                () => reject(new Error("the window never fired")),
                2000,
              ),
            ),
          ]);
          assertEquals(calls, [10], "the window is the value the caller named");
          const failure = failureOf(exit);
          assertEquals(failure.kind, "transport");
          assert(performance.now() - started < 1000, "the window must fire");
          // The window's kind reaches the caller, rather than a generic
          // abort the join made up on its way through.
          assert(
            abortReason instanceof DOMException &&
              abortReason.name === "TimeoutError",
            "the window's own reason reaches fetch",
          );
        });
      });
    } finally {
      AbortSignal.timeout = realTimeout;
    }
  },
);

Deno.test("interrupting the request aborts the fetch", async () => {
  let seen: AbortSignal | undefined;
  await withFetch(
    ((_url: string, init?: RequestInit) => {
      seen = init?.signal ?? undefined;
      return new Promise<Response>(() => {});
    }) as typeof fetch,
    async () => {
      const fiber = Effect.runFork(
        jsonRequest({
          credentials: "include",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      while (seen === undefined) {
        await new Promise((resolve) => setTimeout(resolve, 1));
      }
      assertFalse(seen.aborted, "a live request is not aborted");
      await Effect.runPromise(Fiber.interrupt(fiber));
      assert(seen.aborted, "the caller's interruption must reach fetch");
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
          credentials: "include",
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
          jsonRequest({
            credentials: "include",
            baseUrl: "http://relay",
            path: "/v1/x",
            method,
          }),
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
          credentials: "include",
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
        jsonRequest({
          credentials: "include",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
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

Deno.test("jsonRequest carries a dangling 409 as api data", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            error: { message: "dangling agents", dangling: ["agent-x"] },
          }),
          { status: 409, statusText: "Conflict" },
        ),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        jsonRequest({
          credentials: "omit",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "PUT",
          body: {},
        }),
      );
      const failure = failureOf(exit);
      assert(failure.kind === "api", `expected api, got ${failure.kind}`);
      assertEquals(failure.status, 409);
      assertEquals(failure.message, "dangling agents");
      assertEquals(failure.dangling, ["agent-x"]);
    },
  );
});

Deno.test(
  "jsonRequest maps a refused connection to transport data",
  async () => {
    const cause = new TypeError("connection refused");
    await withFetch((() => Promise.reject(cause)) as typeof fetch, async () => {
      const exit = await Effect.runPromiseExit(
        jsonRequest({
          credentials: "include",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
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
      jsonRequest({
        credentials: "include",
        baseUrl: "http://relay",
        path: "/v1/x",
        method: "GET",
      }),
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
        jsonRequest({
          credentials: "include",
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
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

Deno.test("dataPlaneRequest attaches the session cookie itself", async () => {
  let sent: RequestInit | undefined;
  await withFetch(
    ((_url: string, init?: RequestInit) => {
      sent = init;
      return Promise.resolve(jsonResponse("{}"));
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        dataPlaneRequest({
          baseUrl: "http://relay",
          path: "/v1/x",
          method: "GET",
        }),
      );
      assertEquals(sent?.credentials, "include");
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

Deno.test(
  "runClientEffect rejects with the error instance, not a wrapper",
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
          await runClientEffect(
            dataPlaneRequest<unknown>({
              baseUrl: "http://relay",
              path: "/v1/x",
              method: "GET",
            }),
          );
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

Deno.test(
  "runClientEffect rejects with the signal's reason when it aborts in flight",
  async () => {
    let dialled = 0;
    await withFetch(
      (() => {
        dialled += 1;
        return new Promise<Response>(() => {});
      }) as typeof fetch,
      async () => {
        const external = new AbortController();
        const t0 = performance.now();
        const call = runClientEffect(
          fetchRequest({
            baseUrl: "http://relay",
            path: "/v1/x",
            method: "GET",
            timeoutMs: null,
            credentials: "include",
          }),
          external.signal,
        ).catch((error: unknown) => error);
        await new Promise((resolve) => setTimeout(resolve, 20));
        external.abort();
        // Bounded: a promise face that never settles must fail this leg rather
        // than hang the run.
        const caught = await Promise.race([
          call,
          new Promise<never>((_resolve, reject) =>
            setTimeout(
              () => reject(new Error("the abort never settled")),
              2000,
            ),
          ),
        ]);
        const elapsed = performance.now() - t0;
        // One dial separates this from the pre-abort fast path; the reason
        // identity rules out a wrapper and the elapsed bound rules out
        // "the exchange simply finished".
        assertEquals(dialled, 1);
        assertEquals(caught, external.signal.reason);
        assert(elapsed < 1000, `must settle on the abort: ${elapsed}ms`);
      },
    );
  },
);
