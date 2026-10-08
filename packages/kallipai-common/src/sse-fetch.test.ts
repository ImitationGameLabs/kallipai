// sseFetch behavior: the connect/first-byte window (30s in production,
// parameterized here) aborts a hung connection; it is cleared once response
// headers arrive, so a healthy stream survives past the window; the caller's
// external signal is bridged during the fetch and beats the window; an
// already-aborted external signal fails fast. Legs use real-clock
// microsecond-scale windows (tens of ms) instead of fake-timer infrastructure.

import { assert } from "@std/assert";
import { sseFetch } from "./http.ts";

function withFetch(impl: typeof fetch): () => void {
  const original = globalThis.fetch;
  globalThis.fetch = impl;
  return () => {
    globalThis.fetch = original;
  };
}

Deno.test(
  "sseFetch aborts a hung connection after the connect window",
  async () => {
    const restore = withFetch(((_url: string, init?: RequestInit) => {
      const signal = init?.signal as AbortSignal;
      return new Promise<Response>((_resolve, reject) => {
        signal.addEventListener("abort", () => reject(signal.reason));
      });
    }) as typeof fetch);
    try {
      const t0 = performance.now();
      let error: unknown;
      try {
        await sseFetch("<http://x/sse>", undefined, 50);
      } catch (e) {
        error = e;
      }
      const elapsed = performance.now() - t0;
      assert(error !== undefined, "expected the connect window to abort");
      assert(
        (error as DOMException).name === "AbortError",
        `expected AbortError, got ${String(error)}`,
      );
      assert(elapsed >= 40, `aborted too early: ${elapsed}ms`);
      assert(elapsed < 2000, `aborted too late: ${elapsed}ms`);
    } finally {
      restore();
    }
  },
);

Deno.test(
  "sseFetch clears the window timer on headers: a stream survives past it",
  async () => {
    let streamController:
      | ReadableStreamDefaultController<Uint8Array>
      | undefined;
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        streamController = c;
      },
    });
    const restore = withFetch(((_url: string, init?: RequestInit) => {
      const signal = init?.signal as AbortSignal;
      // Emulate the fetch binding: aborting the fetch signal errors the body.
      signal.addEventListener("abort", () =>
        streamController!.error(new DOMException("aborted", "AbortError")),
      );
      return Promise.resolve(
        new Response(stream, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    }) as typeof fetch);
    try {
      const resp = await sseFetch("<http://x/sse>", undefined, 50);
      const reader = resp.body!.getReader();
      streamController!.enqueue(new TextEncoder().encode("data: first\n\n"));
      const first = await reader.read();
      assert(!first.done);
      // Cross the (50ms) window several times over: had the timer not been
      // cleared, the emulated abort would error the body stream right here.
      await new Promise((r) => setTimeout(r, 150));
      streamController!.enqueue(new TextEncoder().encode("data: second\n\n"));
      const second = await reader.read();
      assert(!second.done, "stream died past the connect window");
      const text = new TextDecoder().decode(second.value);
      assert(
        text.includes("second"),
        `expected the second frame, got: ${text}`,
      );
      reader.cancel();
    } finally {
      restore();
    }
  },
);

Deno.test(
  "sseFetch honors an external abort during the connect window",
  async () => {
    const restore = withFetch(((_url: string, init?: RequestInit) => {
      const signal = init?.signal as AbortSignal;
      return new Promise<Response>((_resolve, reject) => {
        signal.addEventListener("abort", () => reject(signal.reason));
      });
    }) as typeof fetch);
    const external = new AbortController();
    const kicker = setTimeout(() => external.abort(), 10);
    try {
      const t0 = performance.now();
      let error: unknown;
      try {
        await sseFetch("<http://x/sse>", external.signal, 5000);
      } catch (e) {
        error = e;
      }
      const elapsed = performance.now() - t0;
      assert(
        (error as DOMException | undefined)?.name === "AbortError",
        `expected AbortError, got ${String(error)}`,
      );
      assert(
        elapsed < 5000,
        `external abort must beat the connect window, took ${elapsed}ms`,
      );
    } finally {
      clearTimeout(kicker);
      restore();
    }
  },
);

Deno.test(
  "sseFetch fails fast on an already-aborted external signal",
  async () => {
    let sawAbortedSignal = false;
    const restore = withFetch(((_url: string, init?: RequestInit) => {
      const signal = init?.signal as AbortSignal;
      if (signal.aborted) sawAbortedSignal = true;
      return signal.aborted
        ? Promise.reject(signal.reason)
        : Promise.resolve(new Response("data: x\n\n", { status: 200 }));
    }) as typeof fetch);
    try {
      const external = new AbortController();
      external.abort();
      const t0 = performance.now();
      let error: unknown;
      try {
        await sseFetch("<http://x/sse>", external.signal, 50);
      } catch (e) {
        error = e;
      }
      const elapsed = performance.now() - t0;
      assert(sawAbortedSignal, "the pre-aborted signal must reach the fetch");
      assert(
        (error as DOMException | undefined)?.name === "AbortError",
        `expected AbortError, got ${String(error)}`,
      );
      assert(
        elapsed < 40,
        `pre-aborted signal must fail fast, took ${elapsed}ms`,
      );
    } finally {
      restore();
    }
  },
);
