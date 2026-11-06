// Tests for DirectTransport's SSE retry loop (runMux): silent retry,
// resume, exhaustion, watchdog, foreground swap. (The parked-409
// auto-wake tests were removed with the client-side wake dance — the
// server now auto-wakes parked agents on delivery.)

import { assertEquals } from "@std/assert";
import { TagmaClient } from "@kallipai/kallipai-client";
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
  onLiveness: () => void,
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
      onLiveness?: () => void,
    ) {
      state.connects++;
      const next = script.shift();
      if (!next) throw new Error("script exhausted");
      if (next instanceof Error) throw next;
      yield* next(signal, onLiveness!);
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
  async function* (signal, onLiveness) {
    onLiveness(); // connection open
    for (const f of fs) yield f;
    await new Promise<never>((_, reject) => {
      const onAbort = () => reject(new Error("aborted"));
      if (signal?.aborted) onAbort();
      else signal?.addEventListener("abort", onAbort, { once: true });
    });
  };

/** A healthy keepalive-fed connection, modelled at the byte layer: liveness
 * ticks land every few ms (the tagma's `:` comments), independent of any raw
 * frame. With `f` it also yields one frame; without it the stream is quiet
 * but alive. */
const ticking = (f?: RawFrame): AttemptFactory =>
  async function* (signal, onLiveness) {
    const timer = setInterval(() => onLiveness(), 10);
    try {
      yield* liveStream(...(f ? [f] : []))(signal, onLiveness);
    } finally {
      clearInterval(timer);
    }
  };

/** A connection that opens and then goes silent forever (a half-open

* socket): never yields — next() parks until the abort rejects. Hand-rolled
* as an async iterable because a yield-less async generator trips
* require-yield by design. */
const silent: AttemptFactory = (signal, onLiveness) => {
  onLiveness(); // connection open
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
/** A connection that delivers ONE status frame and then goes silent at the
 * byte layer — no raw frames and no keepalive comments, so nothing feeds the
 * watchdog. Strictly quieter than production, and kept inside the shipped
 * watchdog interval: what the leg proves is that the status queue tolerates
 * that silence without reconnecting or closing. */
const oneStatusThenSilence =
  (f: RawFrame): AttemptFactory =>
  (signal, onLiveness) => {
    onLiveness(); // connection open
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
    assertEquals(state.connects, 1, "byte silence holds the stream");
  },
);

Deno.test(
  "keepalive ticks alone hold a quiet stream open past the watchdog",
  async () => {
    const { client, state } = streamClient([ticking()]);
    const states: string[] = [];
    const t = new DirectTransport(
      client,
      "root",
      LOCAL_OPERATOR_SENDER,
      [10_000],
      120, // watchdogMs: twelve keepalive ticks fit inside it
    );
    t.onState = (s) => states.push(s);
    const drain = (async () => {
      for await (const _ of t.status()) {
        /* wait */
      }
    })();
    await tick(500); // four watchdog periods
    t.close();
    await drain;
    assertEquals(state.connects, 1, "keepalives are liveness, not silence");
    assertEquals(states, ["resumed"]);
  },
);

/** A 200 SSE response whose body carries nothing but keepalive comments. */
function keepaliveResponse(ms: number): Response {
  const encoder = new TextEncoder();
  let timer: ReturnType<typeof setInterval> | undefined;
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      timer = setInterval(() => {
        try {
          controller.enqueue(encoder.encode(":\n\n"));
        } catch {
          // closed by cancel(); the timer is cleared there
        }
      }, ms);
    },
    cancel() {
      if (timer !== undefined) clearInterval(timer);
    },
  });
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

Deno.test("a body of keepalive comments alone never reconnects", async () => {
  const original = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(keepaliveResponse(20))) as typeof fetch;
  try {
    const client = new TagmaClient({ baseUrl: "http://tagma.test" });
    const states: string[] = [];
    const t = new DirectTransport(
      client,
      "root",
      LOCAL_OPERATOR_SENDER,
      [10_000],
      200, // watchdogMs: ten keepalive comments fit inside it
    );
    t.onState = (s) => states.push(s);
    const drain = (async () => {
      for await (const _ of t.status()) {
        /* wait */
      }
    })();
    await tick(700); // three and a half watchdog periods
    t.close();
    await drain;
    assertEquals(
      states,
      ["resumed"],
      "keepalive bytes are liveness: no reconnecting inside three periods",
    );
  } finally {
    globalThis.fetch = original;
  }
});

Deno.test("a missed frame decodes with no payload", async () => {
  // The marker says frames were lost; it names no position, because the
  // reader's own cursor already says where to resume. An empty payload must
  // therefore read as the marker, not as a frame the demux could not decode.
  const { client } = streamClient([
    liveStream({ event: "missed", data: "{}" }),
  ]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  let missed = 0;
  t.onMissed = () => {
    missed += 1;
  };
  const drain = (async () => {
    for await (const _ of t.status()) {
      /* wait */
    }
  })();
  await tick(80);
  t.close();
  await drain;
  assertEquals(missed, 1);
});

/** The closing marker as it arrives on the wire: a named event, empty JSON. */
const closingFrame: RawFrame = { event: "closing", data: "{}" };

Deno.test("a planned close reconnects at once and fails nothing", async () => {
  const { client, state } = streamClient([
    liveStream(statusFrame(1), closingFrame),
    liveStream(statusFrame(2)),
  ]);
  const states: string[] = [];
  // A minute of backoff: anything but a maintenance reconnect would park
  // here and the drain would never see the second connection.
  const t = new DirectTransport(
    client,
    "root",
    LOCAL_OPERATOR_SENDER,
    [60_000],
  );
  t.onState = (s) => states.push(s);
  const seen: number[] = [];
  const drain = (async () => {
    for await (const s of t.status()) seen.push(s.subagentsTotal);
  })();
  await tick(60);
  t.close();
  await drain;

  assertEquals(state.connects, 2, "the planned close reconnects immediately");
  assertEquals(seen, [1, 2], "the status queue survives the planned close");
  assertEquals(
    states,
    ["resumed", "resumed"],
    "no failure banner for a shutdown",
  );
});

Deno.test("frames after the closing mark are dropped", async () => {
  // A stuck writer (or a middlebox) can keep sending after `closing`. The
  // client must leave on the mark instead of draining the body, so these
  // trailing frames never reach a queue.
  const { client, state } = streamClient([
    liveStream(statusFrame(1), closingFrame, statusFrame(2), statusFrame(3)),
    liveStream(statusFrame(4)),
  ]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  const seen: number[] = [];
  const drain = (async () => {
    for await (const s of t.status()) seen.push(s.subagentsTotal);
  })();
  await tick(80);
  t.close();
  await drain;

  assertEquals(seen, [1, 4], "nothing past the closing mark is delivered");
  assertEquals(
    state.connects,
    2,
    "the client reconnected instead of reading on",
  );
});

Deno.test("repeated planned closes reconnect at once", async () => {
  // The shape a restarting tagma produces: the connect snapshot, the
  // mark, then the connection goes away. Every attempt after that is
  // immediate, and none of it spends the retry budget or reaches the
  // reconnect banner.
  const { client, state } = streamClient([
    liveStream(statusFrame(1), closingFrame),
    liveStream(statusFrame(2), closingFrame),
    liveStream(statusFrame(3)),
  ]);
  const states: string[] = [];
  const t = new DirectTransport(
    client,
    "root",
    LOCAL_OPERATOR_SENDER,
    [60_000],
  );
  t.onState = (s) => states.push(s);
  const seen: number[] = [];
  const drain = (async () => {
    for await (const s of t.status()) seen.push(s.subagentsTotal);
  })();
  await tick(80);
  t.close();
  await drain;

  assertEquals(state.connects, 3, "each planned close reconnects at once");
  assertEquals(seen, [1, 2, 3], "every connect snapshot reaches the queue");
  assertEquals(states, ["resumed", "resumed", "resumed"], "no banner");
});

Deno.test(
  "a clean close without a closing mark is a fault, not a handoff",
  async () => {
    // The mark is the only thing that makes a close planned: a body that just
    // ends keeps the failure path, so the user sees the reconnect state.
    const endsClean: AttemptFactory = (_signal, onLiveness) => {
      onLiveness(); // connection open, then the body ends with no frames
      return {
        [Symbol.asyncIterator]: () => ({
          next: (): Promise<IteratorResult<RawFrame>> =>
            Promise.resolve({ done: true, value: undefined }),
        }),
      };
    };
    const { client } = streamClient([
      endsClean,
      endsClean,
      liveStream(statusFrame(9)),
    ]);
    const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
    const states: string[] = [];
    t.onState = (s) => states.push(s);
    const seen: number[] = [];
    const drain = (async () => {
      for await (const s of t.status()) seen.push(s.subagentsTotal);
    })();
    await tick(80);
    t.close();
    await drain;

    assertEquals(seen, [9], "the third connection carries the stream");
    assertEquals(
      states,
      ["resumed", "reconnecting", "resumed", "reconnecting", "resumed"],
      "each unannounced clean end spends a ladder step",
    );
  },
);

Deno.test("a closing frame decodes with no payload", async () => {
  const { client } = streamClient([liveStream(closingFrame)]);
  const t = new DirectTransport(
    client,
    "root",
    LOCAL_OPERATOR_SENDER,
    [10_000],
  );
  const kinds: string[] = [];
  const drain = (async () => {
    try {
      for await (const f of t.frames(undefined, () => {})) kinds.push(f.kind);
    } catch {
      // close() aborts the underlying stream; the decode is what this pins.
    }
  })();
  await tick(30);
  t.close();
  await drain;

  assertEquals(kinds, ["closing"]);
});
