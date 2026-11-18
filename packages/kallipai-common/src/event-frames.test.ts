// eventFrames: the `/me/events` machine as a stream. A refusal is a
// `DataPlaneApiError` in the failure channel, a body that dies or a frame that
// is not JSON is a `TransportError`, and the caller's signal ends the stream
// cleanly after tearing the body down. The generator face (`runClientStream`)
// hands those failures to the iterator as the error class itself, which is
// what the reconnect loop's 401 branch matches on.

import { assert, assertEquals } from "@std/assert";
import { Cause, Effect, Exit, Fiber, Result, Stream } from "effect";
import { DataPlaneApiError, TransportError } from "./errors.ts";
import {
  type EventFrame,
  eventFrames,
  runClientEffect,
  runClientStream,
  SSE_CONNECT_TIMEOUT_MESSAGE,
  sseRequest,
} from "./http.ts";
import { withFetch } from "./testing.ts";

const encoder = new TextEncoder();

/** A body that emits `chunks`, then closes. */
function bodyOf(chunks: string[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
}

/** The typed failure of an exit, asserting it is not a defect. */
function failureOf<A, E>(exit: Exit.Exit<A, E>): E {
  assert(Exit.isFailure(exit), "expected a failure");
  const found = Cause.findError(exit.cause);
  assert(Result.isSuccess(found), "expected a typed failure, not a defect");
  return found.success;
}

const sseHeaders = { "content-type": "text/event-stream" };

Deno.test("eventFrames yields the marker and the cursor frames", async () => {
  const body =
    `event: stream\nid: 0\ndata: ${JSON.stringify({ epoch: 3, next_seq: 9 })}\n\n` +
    `event: tagma_online\nid: 3:7\ndata: ${JSON.stringify({ n: 1 })}\n\n` +
    `event: tagma_offline\ndata: ${JSON.stringify({ n: 2 })}\n\n`;
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(body, { status: 200, headers: sseHeaders }),
      )) as typeof fetch,
    async () => {
      const frames = await Effect.runPromise(
        Stream.runCollect(eventFrames<{ n: number }>("http://x/sse")),
      );
      assertEquals(frames, [
        { kind: "stream", epoch: 3, nextSeq: 9 },
        { kind: "event", epoch: 3, seq: 7, event: { n: 1 } },
        { kind: "event", epoch: null, seq: null, event: { n: 2 } },
      ]);
    },
  );
});

Deno.test("a caller header cannot replace the stream's accept", async () => {
  let sent: Record<string, string> | undefined;
  await withFetch(
    ((_url: string | URL, init?: RequestInit) => {
      sent = init?.headers as Record<string, string>;
      return Promise.resolve(
        new Response("", { status: 200, headers: sseHeaders }),
      );
    }) as typeof fetch,
    async () => {
      await Effect.runPromise(
        sseRequest("http://x/sse", {
          credentials: "omit",
          headers: { accept: "application/json" },
        }),
      );
      // The stream's own contract wins: extra headers are additive.
      assertEquals(sent?.accept, "text/event-stream");
    },
  );
});

Deno.test("a refusal is a value in the failure channel", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        Response.json(
          { error: { message: "not a member", code: "not_member" } },
          { status: 403 },
        ),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        Stream.runCollect(eventFrames("http://x/sse")),
      );
      const failure = failureOf(exit);
      assert(failure instanceof DataPlaneApiError);
      assertEquals(failure.status, 403);
      assertEquals(failure.message, "not a member");
      assertEquals(failure.code, "not_member");
    },
  );
});

Deno.test("a body that dies mid-stream fails as a TransportError", async () => {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(
        encoder.encode(`data: ${JSON.stringify({ n: 1 })}\n\n`),
      );
      controller.error(new TypeError("connection lost"));
    },
  });
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(body, { status: 200, headers: sseHeaders }),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        Stream.runCollect(eventFrames("http://x/sse")),
      );
      const failure = failureOf(exit);
      assert(failure instanceof TransportError);
    },
  );
});

Deno.test("a frame that is not JSON fails as a TransportError", async () => {
  await withFetch(
    (() =>
      Promise.resolve(
        new Response(bodyOf(["data: {oops\n\n"]), {
          status: 200,
          headers: sseHeaders,
        }),
      )) as typeof fetch,
    async () => {
      const exit = await Effect.runPromiseExit(
        Stream.runCollect(eventFrames("http://x/sse")),
      );
      const failure = failureOf(exit);
      assert(failure instanceof TransportError);
    },
  );
});

Deno.test(
  "runClientStream hands the refusal to the iterator as its class",
  async () => {
    await withFetch(
      (() =>
        Promise.resolve(
          Response.json(
            { error: { message: "session expired" } },
            {
              status: 401,
            },
          ),
        )) as typeof fetch,
      async () => {
        const frames = runClientStream<EventFrame<unknown>, unknown>(
          eventFrames("http://x/sse"),
        );
        let error: unknown;
        try {
          for await (const _frame of frames) {
            // A refusal arrives before any frame.
          }
        } catch (e) {
          error = e;
        }
        // The reconnect loop branches on exactly this shape.
        assert(error instanceof DataPlaneApiError, `got ${String(error)}`);
        assertEquals(error.status, 401);
      },
    );
  },
);

Deno.test(
  "the caller's signal ends the stream cleanly and cancels the body",
  async () => {
    let cancelled = false;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          encoder.encode(`data: ${JSON.stringify({ n: 1 })}\n\n`),
        );
      },
      cancel() {
        cancelled = true;
      },
    });
    await withFetch(
      (() =>
        Promise.resolve(
          new Response(body, { status: 200, headers: sseHeaders }),
        )) as typeof fetch,
      async () => {
        const external = new AbortController();
        const frames = runClientStream<EventFrame<{ n: number }>, unknown>(
          eventFrames("http://x/sse"),
          external.signal,
        );
        const first = await frames.next();
        assertEquals(first.value, {
          kind: "event",
          epoch: null,
          seq: null,
          event: { n: 1 },
        });
        external.abort();
        const t0 = performance.now();
        let second: IteratorResult<EventFrame<{ n: number }>>;
        try {
          second = await frames.next();
        } catch (e) {
          throw new Error(
            `an abort must end the stream, not throw: ${String(e)}`,
          );
        }
        assert(second.done === true, "the abort must end the stream");
        assert(performance.now() - t0 < 1000, "the abort must settle promptly");
        assert(cancelled, "the abort must cancel the body");
      },
    );
  },
);

Deno.test(
  "an abort while the next frame is pending ends the stream",
  async () => {
    let cancelled = false;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(
          encoder.encode(`data: ${JSON.stringify({ n: 1 })}\n\n`),
        );
        // Then silence: no further frame and no close, so the parser is parked
        // on a read that is not coming back on its own.
      },
      cancel() {
        cancelled = true;
      },
    });
    await withFetch(
      (() =>
        Promise.resolve(
          new Response(body, { status: 200, headers: sseHeaders }),
        )) as typeof fetch,
      async () => {
        const external = new AbortController();
        const frames = runClientStream<EventFrame<{ n: number }>, unknown>(
          eventFrames("http://x/sse"),
          external.signal,
        );
        const first = await frames.next();
        assert(first.done === false, "the first frame must arrive");
        // The read is pending on a quiet stream when the abort lands: the
        // shape a long-lived stream actually ends in.
        const pending = frames.next();
        await new Promise((resolve) => setTimeout(resolve, 20));
        external.abort();
        const t0 = performance.now();
        const second = await pending;
        assert(second.done === true, "the abort must end the stream");
        assert(performance.now() - t0 < 1000, "the abort must settle promptly");
        assert(cancelled, "the abort must cancel the body");
      },
    );
  },
);

/** A promise bounded at two seconds: a window that never fires must fail the
 * leg, because a hung test has no discriminative power. */
function within<T>(run: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const bound = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(label)), 2000);
  });
  return Promise.race([run, bound]).finally(() => clearTimeout(timer));
}

Deno.test(
  "the connect window fails a connection that never answers",
  async () => {
    let sawAbort = false;
    await withFetch(
      ((_url: string | URL, init?: RequestInit) => {
        const signal = init?.signal as AbortSignal;
        return new Promise<Response>((_resolve, reject) => {
          signal.addEventListener("abort", () => {
            sawAbort = true;
            reject(signal.reason);
          });
        });
      }) as typeof fetch,
      async () => {
        const t0 = performance.now();
        const exit = await within(
          Effect.runPromiseExit(
            sseRequest("http://x/sse", {
              credentials: "include",
              connectTimeoutMs: 50,
            }),
          ),
          "the window never fired",
        );
        const elapsed = performance.now() - t0;
        const failure = failureOf(exit);
        assert(
          failure instanceof TransportError,
          `expected a TransportError, got ${String(failure)}`,
        );
        assertEquals(failure.message, SSE_CONNECT_TIMEOUT_MESSAGE);
        assert(sawAbort, "the window must reach the fetch as an abort");
        assert(elapsed >= 40, `aborted too early: ${elapsed}ms`);
        assert(elapsed < 2000, `aborted too late: ${elapsed}ms`);
      },
    );
  },
);

Deno.test(
  "the connect window ends at the headers: the body outlives it",
  async () => {
    let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        controller = c;
      },
    });
    await withFetch(
      ((_url: string | URL, init?: RequestInit) => {
        const signal = init?.signal as AbortSignal;
        // Emulate the fetch binding: aborting the fetch signal errors the body.
        signal.addEventListener("abort", () =>
          controller!.error(new DOMException("aborted", "AbortError")),
        );
        return Promise.resolve(
          new Response(stream, { status: 200, headers: sseHeaders }),
        );
      }) as typeof fetch,
      async () => {
        const resp = await within(
          Effect.runPromise(
            sseRequest("http://x/sse", {
              credentials: "include",
              connectTimeoutMs: 50,
            }),
          ),
          "the headers never arrived",
        );
        const reader = resp.body!.getReader();
        controller!.enqueue(encoder.encode("data: first\n\n"));
        const first = await within(reader.read(), "the first read hung");
        assert(!first.done);
        // Cross the (50ms) window several times over: had the timer not been
        // cleared at the headers, the emulated abort would error the body here.
        await new Promise((resolve) => setTimeout(resolve, 150));
        controller!.enqueue(encoder.encode("data: second\n\n"));
        const second = await within(reader.read(), "the second read hung");
        assert(!second.done, "stream died past the connect window");
        assert(new TextDecoder().decode(second.value).includes("second"));
        await reader.cancel();
      },
    );
  },
);

Deno.test(
  "an abort inside the connect window is the abort, not the window",
  async () => {
    let dialled = 0;
    await withFetch(
      (() => {
        dialled += 1;
        return new Promise<Response>(() => {});
      }) as typeof fetch,
      async () => {
        // Five seconds of window: had `timeoutOrElse` swallowed the interrupt
        // into its own failure, this would settle as the window message.
        const fiber = Effect.runFork(
          sseRequest("http://x/sse", {
            credentials: "include",
            connectTimeoutMs: 5_000,
          }),
        );
        await new Promise((resolve) => setTimeout(resolve, 25));
        const t0 = performance.now();
        await within(
          Effect.runPromise(Fiber.interrupt(fiber)),
          "the interrupt never landed",
        );
        // The interrupted fiber's own exit is where the reason lives: a
        // swallowed interrupt would surface as the window's failure here.
        const exit = await Effect.runPromiseExit(Fiber.join(fiber));
        const elapsed = performance.now() - t0;
        assertEquals(dialled, 1);
        assert(Exit.isFailure(exit), "the interrupt must end the exchange");
        assert(
          Cause.hasInterruptsOnly(exit.cause),
          "an abort is an interruption, not the window's failure",
        );
        assert(
          elapsed < 1000,
          `the interrupt must not wait out the window: ${elapsed}ms`,
        );
      },
    );
  },
);

Deno.test("an already-aborted caller dials nothing", async () => {
  let dialled = 0;
  await withFetch(
    (() => {
      dialled += 1;
      return Promise.resolve(new Response("", { status: 200 }));
    }) as typeof fetch,
    async () => {
      const external = new AbortController();
      external.abort();
      let caught: unknown;
      try {
        await runClientEffect(
          sseRequest("http://x/sse", {
            credentials: "include",
            connectTimeoutMs: 5_000,
          }),
          external.signal,
        );
      } catch (error) {
        caught = error;
      }
      assertEquals(dialled, 0);
      // The contract at the Promise boundary: the signal's own reason, not a
      // fresh error and not a timeout message.
      assertEquals(caught, external.signal.reason);
    },
  );
});
