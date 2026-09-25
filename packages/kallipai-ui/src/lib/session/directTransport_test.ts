// Tests for DirectTransport's SSE retry loop (runMux): silent retry,
// resume, exhaustion, watchdog, foreground swap. (The parked-409
// auto-wake tests were removed with the client-side wake dance — the
// server now auto-wakes parked agents on delivery.)

import { assertEquals } from "@std/assert";
import type { TagmaClient } from "@kallipai/kallipai-client";
import { DirectTransport } from "./directTransport.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

// --- the SSE retry loop (runMux) ---

/** One raw frame as the demux sees it. */
type RawFrame = { readonly event: string; readonly data: string };

/** A scripted stream attempt: an Error rejects the connect, a factory

* yields the frames of one connection (mirroring the real client, which
* reports liveness when the connection opens). */
type AttemptFactory = (
  signal: AbortSignal | undefined,
  onFrame: () => void,
) => AsyncIterable<RawFrame>;

function streamClient(script: (Error | AttemptFactory)[]): {
  client: TagmaClient;
  state: { connects: number };
} {
  const state = { connects: 0 };
  const client = {
    async *externalEventStream(
      _id: string,
      signal?: AbortSignal,
      onFrame?: () => void,
    ) {
      state.connects++;
      const next = script.shift();
      if (!next) throw new Error("script exhausted");
      if (next instanceof Error) throw next;
      yield* next(signal, onFrame!);
    },
  } as unknown as TagmaClient;
  return { client, state };
}

function statusFrame(n: number): RawFrame {
  return {
    event: "status",
    data: JSON.stringify({
      root_state: "idle",
      subagents_total: n,
      subagents_active: 0,
      token_budget: 0,
      token_consumed: 0,
    }),
  };
}

/** A healthy connection: yields the given frames, then holds open (like a

* live SSE) until aborted. Ending the generator would end the stream
* cleanly, which now triggers a reconnect. */
const liveStream = (...fs: RawFrame[]): AttemptFactory =>
  async function* (signal, onFrame) {
    onFrame(); // connection open
    for (const f of fs) yield f;
    await new Promise<never>((_, reject) => {
      const onAbort = () => reject(new Error("aborted"));
      if (signal?.aborted) onAbort();
      else signal?.addEventListener("abort", onAbort, { once: true });
    });
  };

/** A healthy keepalive-fed connection: yields the frame every few ms (so

* the watchdog stays fed) until aborted. */
const ticking = (f: RawFrame): AttemptFactory =>
  async function* (sig, onFrame) {
    onFrame(); // connection open
    for (;;) {
      onFrame(); // every raw frame feeds the watchdog
      yield f;
      const aborted = await new Promise<boolean>((resolve) => {
        const t = setTimeout(() => resolve(false), 5);
        const onAbort = () => {
          clearTimeout(t);
          resolve(true);
        };
        if (sig?.aborted) onAbort();
        else sig?.addEventListener("abort", onAbort, { once: true });
      });
      if (aborted) return;
    }
  };

/** A connection that opens and then goes silent forever (a half-open

* socket): never yields — next() parks until the abort rejects. Hand-rolled
* as an async iterable because a yield-less async generator trips
* require-yield by design. */
const silent: AttemptFactory = (signal, onFrame) => {
  onFrame(); // connection open
  return {
    [Symbol.asyncIterator]: () => ({
      next: (): Promise<IteratorResult<RawFrame>> =>
        new Promise<never>((_, reject) => {
          const onAbort = () => reject(new Error("aborted"));
          if (signal?.aborted) onAbort();
          else signal?.addEventListener("abort", onAbort, { once: true });
        }),
    }),
  };
};

const tick = (ms: number) => new Promise((r) => setTimeout(r, ms));
/** A connection that delivers ONE status frame and then goes completely
 * silent — no raw frames and no keepalive comments. Strictly quieter
 * than production (where SSE comments feed the watchdog; that path is
 * outside unit-test timescale). What the leg proves: the status queue
 * tolerates frame silence without reconnecting or closing. */
const oneStatusThenSilence =
  (f: RawFrame): AttemptFactory =>
  (signal, onFrame) => {
    onFrame(); // connection open
    let yielded = false;
    return {
      [Symbol.asyncIterator]: () => ({
        next: (): Promise<IteratorResult<RawFrame>> => {
          if (!yielded) {
            yielded = true;
            return Promise.resolve({ value: f, done: false });
          }
          return new Promise<never>((_, reject) => {
            const onAbort = () => reject(new Error("aborted"));
            if (signal?.aborted) onAbort();
            else signal?.addEventListener("abort", onAbort, { once: true });
          });
        },
      }),
    };
  };

Deno.test("stream failure retries silently and resumes", async () => {
  const { client, state } = streamClient([
    new Error("net down"),
    liveStream(statusFrame(1), statusFrame(2)),
  ]);
  const states: string[] = [];
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  t.onState = (s) => states.push(s);
  const seen: number[] = [];
  const drain = (async () => {
    for await (const s of t.status()) seen.push(s.subagentsTotal);
  })();
  await tick(80);
  t.close();
  await drain;
  assertEquals(state.connects, 2);
  assertEquals(states, ["reconnecting", "resumed"]);
  assertEquals(seen, [1, 2]);
});

Deno.test("stream retries exhaust and fail the drains", async () => {
  const { client, state } = streamClient([
    new Error("a"),
    new Error("b"),
    new Error("c"),
  ]);
  const states: string[] = [];
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1, 1]);
  t.onState = (s) => states.push(s);
  let err: unknown = null;
  try {
    for await (const _ of t.status()) break;
  } catch (e) {
    err = e;
  }
  assertEquals((err as Error).message, "c");
  assertEquals(state.connects, 3);
  assertEquals(states, ["reconnecting", "reconnecting"]);
});

Deno.test(
  "watchdog tears down a silent half-open stream and reconnects",
  async () => {
    const { client, state } = streamClient([silent, ticking(statusFrame(7))]);
    const states: string[] = [];
    const t = new DirectTransport(
      client,
      "root",
      LOCAL_OPERATOR_SENDER,
      [10_000],
      30,
    );
    t.onState = (s) => states.push(s);
    const seen: number[] = [];
    const drain = (async () => {
      for await (const s of t.status()) seen.push(s.subagentsTotal);
    })();
    await tick(120);
    t.close();
    await drain;
    assertEquals(state.connects, 2);
    assertEquals(states, ["resumed", "reconnecting", "resumed"]);
    assertEquals(seen.length >= 1 && seen.every((v) => v === 7), true);
  },
);

Deno.test(
  "close during reconnect backoff ends the drains cleanly",
  async () => {
    const { client, state } = streamClient([new Error("down")]);
    const states: string[] = [];
    const t = new DirectTransport(
      client,
      "root",
      LOCAL_OPERATOR_SENDER,
      [60_000],
    );
    t.onState = (s) => states.push(s);
    let ended = false;
    const drain = (async () => {
      for await (const _ of t.status()) {
        /* wait */
      }
      ended = true;
    })();
    await tick(30); // first failure -> reconnecting -> sleeping 60s
    t.close(); // must kick the sleep
    await drain;
    assertEquals(ended, true);
    assertEquals(states, ["reconnecting"]);
    assertEquals(state.connects, 1);
  },
);

Deno.test("foreground return swaps the stream silently", async () => {
  const { client, state } = streamClient([silent, liveStream(statusFrame(3))]);
  const states: string[] = [];
  const t = new DirectTransport(
    client,
    "root",
    LOCAL_OPERATOR_SENDER,
    [10_000],
  );
  t.onState = (s) => states.push(s);
  const drain = (async () => {
    for await (const _ of t.status()) {
      /* wait */
    }
  })();
  await tick(20); // connected and silent
  t.handleForegroundVisible(); // no DOM in Deno: treated as visible
  await tick(40); // swap -> frames flow
  t.close();
  await drain;
  assertEquals(state.connects, 2);
  assertEquals(states, ["resumed", "resumed"]); // no reconnecting flash
});

Deno.test(
  "sparse status: one change-driven frame then total silence stays open",
  async () => {
    // The direct differential unification (the declared behavior
    // change) means the frontend no longer sees a status frame every
    // ~2 s. Pin the tolerance: one frame lands, then the stream goes
    // fully silent (no raw frames — stricter than production's keepalive
    // cadence) — no reconnect, no queue close, no staleness watchdog
    // trip. The header just holds its last snapshot.
    const { client, state } = streamClient([
      oneStatusThenSilence(statusFrame(1)),
    ]);
    const t = new DirectTransport(
      client,
      "root",
      LOCAL_OPERATOR_SENDER,
      [10_000],
    );
    const seen: number[] = [];
    const drain = (async () => {
      for await (const s of t.status()) seen.push(s.subagentsTotal);
    })();
    await tick(80);
    t.close();
    await drain;
    assertEquals(seen, [1]);
    assertEquals(state.connects, 1, "frame silence holds the stream");
  },
);
