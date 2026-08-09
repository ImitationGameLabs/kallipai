// Tests for the Transport implementations. The DirectTransport fan-out (one SSE
// demuxed into two queues) and the RelayTransport signal queue are the two
// pieces of new plumbing; both are testable in isolation with fake clients.

import { assertEquals, assertRejects } from "@std/assert";
import { AsyncQueue, type IncomingFrame } from "./transport.ts";
import {
  DirectTransport,
  type DirectAuthoredPayload,
} from "./directTransport.ts";
import { RelayTransport } from "./relayTransport.ts";
import type { TagmaClient } from "@kallipai/kallip-client";
import type {
  Participant,
  RelayChannel,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallip-lesche-client";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

// Shared fixtures: the agent that authors replies on the wire, and the local
// operator sender the transports render for optimistic user lines.
const participant: Participant = {
  kind: "agent",
  id: "root",
  handle: "Agent",
};
const localSender = LOCAL_OPERATOR_SENDER;

// --- AsyncQueue ---

Deno.test(
  "AsyncQueue buffers items pushed before the consumer drains",
  async () => {
    const q = new AsyncQueue<number>();
    q.push(1);
    q.push(2);
    assertEquals((await q.next()).value, 1);
    assertEquals((await q.next()).value, 2);
  },
);

Deno.test(
  "AsyncQueue delivers items pushed while the consumer awaits",
  async () => {
    const q = new AsyncQueue<number>();
    const got: number[] = [];
    const drain = (async () => {
      for (let i = 0; i < 2; i++) {
        const { value, done } = await q.next();
        if (done) break;
        got.push(value);
      }
    })();
    q.push(10);
    q.push(20);
    await drain;
    assertEquals(got, [10, 20]);
  },
);

Deno.test("AsyncQueue close ends the drain with done", async () => {
  const q = new AsyncQueue<number>();
  q.close();
  assertEquals((await q.next()).done, true);
});

Deno.test("AsyncQueue close drops buffered items", async () => {
  const q = new AsyncQueue<number>();
  q.push(1);
  q.push(2);
  q.close();
  assertEquals((await q.next()).done, true); // 1 and 2 are not delivered
});

Deno.test("AsyncQueue close resolves a pending next with done", async () => {
  const q = new AsyncQueue<number>();
  const p = q.next();
  q.close();
  assertEquals((await p).done, true);
});

Deno.test("AsyncQueue fail rejects a pending and a later next", async () => {
  const q = new AsyncQueue<number>();
  const p = q.next();
  q.fail(new Error("boom"));
  await assertRejects(() => p, "boom");
  // A later consumer also sees the failure (the drain throws, not ends).
  await assertRejects(() => q.next(), "boom");
});

// --- helpers ---

async function drain<T>(gen: AsyncGenerator<T>): Promise<T[]> {
  const out: T[] = [];
  for await (const v of gen) out.push(v);
  return out;
}

/** A fake TagmaClient whose externalEventStream yields the given raw
 * `{event,data}` frames in order, then ends. */
function fakeClient(frames: { event: string; data: string }[]): TagmaClient {
  return {
    externalEventStream: async function* () {
      for (const f of frames) yield f;
    },
  } as unknown as TagmaClient;
}

const authored = (
  content: string,
  history_id = 0,
): {
  event: string;
  data: string;
} => ({
  event: "authored",
  // The direct `authored` SSE data is the sender paired with the content reply
  // (DirectAuthoredPayload), not a bare TagmaReply -- the offline path has no
  // relay envelope, so the direct frame carries the sender itself.
  data: JSON.stringify({
    sender: participant,
    reply: {
      kind: "event",
      event: { type: "assistant_content", content },
      history_id,
    },
  } satisfies DirectAuthoredPayload),
});

const signal = (ev: SignalEvent): { event: string; data: string } => ({
  event: "signal",
  data: JSON.stringify(ev),
});

const status = (): { event: string; data: string } => ({
  event: "status",
  data: JSON.stringify({
    root_state: "idle",
    subagents_total: 0,
    subagents_active: 0,
    token_budget: 0,
    token_consumed: 0,
  }),
});

// --- DirectTransport ---

Deno.test(
  "DirectTransport demuxes one SSE into replies + signals + status",
  async () => {
    const t = new DirectTransport(
      fakeClient([
        signal({ type: "busy" }),
        authored("hello", 1),
        signal({ type: "idle" }),
        status(),
        authored("world", 2),
      ]),
      "root",
      localSender,
    );
    const [replies, signals, statuses] = await Promise.all([
      drain(t.replies()),
      drain(t.signals()),
      drain(t.status()),
    ]);
    assertEquals(
      replies.map((r) => r.reply.kind),
      ["event", "event"],
    );
    assertEquals(
      (replies[0]!.reply as Extract<TagmaReply, { kind: "event" }>).event
        .content,
      "hello",
    );
    assertEquals(
      signals.map((s) => s.type),
      ["busy", "idle"],
    );
    // The status frame is surfaced (mapped snake_case -> TagmaStatusSummary),
    // not dropped: the chat header reads it off the conversation.
    assertEquals(statuses.length, 1);
    assertEquals(statuses[0]!.rootState, "idle");
  },
);

Deno.test(
  "DirectTransport propagates a mid-stream failure to ALL THREE drains",
  async () => {
    const t = new DirectTransport(
      {
        externalEventStream: async function* () {
          yield signal({ type: "busy" });
          throw new Error("wire broke");
        },
      } as unknown as TagmaClient,
      "root",
      localSender,
    );
    const replyP = drain(t.replies());
    const signalP = drain(t.signals());
    const statusP = drain(t.status());
    // All three reject with the wire failure.
    await assertRejects(() => replyP, "wire broke");
    await assertRejects(() => signalP, "wire broke");
    await assertRejects(() => statusP, "wire broke");
  },
);

Deno.test(
  "DirectTransport starts the demux exactly once for concurrent drains",
  async () => {
    let starts = 0;
    const t = new DirectTransport(
      {
        externalEventStream: async function* () {
          starts++;
          yield authored("a", 1);
          yield signal({ type: "busy" });
        },
      } as unknown as TagmaClient,
      "root",
      localSender,
    );
    const [replies, signals] = await Promise.all([
      drain(t.replies()),
      drain(t.signals()),
    ]);
    assertEquals(starts, 1, "the SSE is opened once even with two drains");
    assertEquals(replies.length, 1);
    assertEquals(signals.length, 1);
  },
);

Deno.test(
  "DirectTransport ends both drains on a clean stream end",
  async () => {
    const t = new DirectTransport(
      fakeClient([authored("hi", 1)]),
      "root",
      localSender,
    );
    const [replies, signals] = await Promise.all([
      drain(t.replies()),
      drain(t.signals()),
    ]);
    assertEquals(replies.length, 1);
    assertEquals(signals, []);
  },
);

Deno.test("DirectTransport close ends both drains", async () => {
  // A stream that yields three frames then blocks forever (a live SSE with no
  // further frames). close() must terminate both drains without waiting for the
  // stream to end on its own.
  const t = new DirectTransport(
    {
      externalEventStream: async function* () {
        yield authored("a", 1);
        yield authored("b", 2);
        yield authored("c", 3);
        // Block forever, like an idle live SSE.
        await new Promise<void>(() => {});
      },
    } as unknown as TagmaClient,
    "root",
    localSender,
  );
  const replies: IncomingFrame[] = [];
  const draining = (async () => {
    for await (const r of t.replies()) {
      replies.push(r);
      if (replies.length === 3) t.close();
    }
  })();
  await draining;
  assertEquals(replies.length, 3, "drain consumed all 3 before close");
  // signals() also ends.
  assertEquals(await drain(t.signals()), []);
});

// --- RelayTransport ---

/** A fake RelayChannel whose replies() yields the given IncomingFrames, then
 * ends. `localParticipant` is read by the RelayTransport constructor (it derives
 * `localSender` from it), so the fake must supply one. */
function fakeChannel(replies: IncomingFrame[]): RelayChannel {
  return {
    localParticipant: participant,
    replies: async function* () {
      for (const r of replies) yield r;
    },
    send: () => Promise.resolve(),
    close: () => {},
  } as unknown as RelayChannel;
}

Deno.test("RelayTransport replies delegate to the RelayChannel", async () => {
  const frame: IncomingFrame = {
    sender: participant,
    reply: {
      kind: "event",
      event: { type: "assistant_content", content: "hi" },
      history_id: 1,
    },
  };
  const t = new RelayTransport(fakeChannel([frame]));
  assertEquals(await drain(t.replies()), [frame]);
});

Deno.test(
  "RelayTransport signals drain the queue fed by enqueueSignal",
  async () => {
    const t = new RelayTransport(fakeChannel([]));
    const got: SignalEvent[] = [];
    const draining = (async () => {
      for await (const s of t.signals()) {
        got.push(s);
        if (got.length === 2) break;
      }
    })();
    t.enqueueSignal({ type: "busy" });
    t.enqueueSignal({ type: "idle" });
    await draining;
    assertEquals(
      got.map((s) => s.type),
      ["busy", "idle"],
    );
  },
);

Deno.test("RelayTransport close ends the signal drain", async () => {
  const t = new RelayTransport(fakeChannel([]));
  t.close();
  assertEquals(await drain(t.signals()), []);
});

Deno.test(
  "RelayTransport replies ending/throwing closes the signal drain (no hang)",
  async () => {
    // If the E2EE reply stream throws (e.g. a malformed AEAD-valid payload),
    // the signal queue must still close so the signals drain ends -- otherwise
    // a Conversation.run() (Promise.allSettled) would hang forever.
    const t = new RelayTransport({
      localParticipant: participant,
      replies: async function* () {
        yield {
          sender: participant,
          reply: {
            kind: "event",
            event: { type: "assistant_content", content: "x" },
            history_id: 1,
          },
        };
        throw new Error("bad payload");
      },
      send: () => Promise.resolve(),
      close: () => {},
    } as unknown as RelayChannel);
    const replies: IncomingFrame[] = [];
    await assertRejects(async () => {
      for await (const r of t.replies()) replies.push(r);
    }, "bad payload");
    assertEquals(replies.length, 1);
    // The signal drain ends cleanly (does not hang) once replies threw.
    assertEquals(await drain(t.signals()), []);
  },
);
