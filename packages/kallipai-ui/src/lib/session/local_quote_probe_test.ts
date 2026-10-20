// The local leaf's quote probe: a quoted row the loaded window does not hold
// is resolved with one before-page straight off the transport. That page is
// all this leaf contributes -- the split between a contiguous miss (`deleted`)
// and a read the client could not answer (`error`, retryable) is the base's
// single decision, and the leaf answers null for a probe it cannot serve.
//
// The module under test is rune-bearing and `deno test` runs it uncompiled, so
// a passthrough `$state` shim lets it run with plain fields.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { LocalConversation } = await import("./conversation.svelte.ts");
import { DirectTransport } from "./directTransport.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

type PageRequest = {
  after?: number | null;
  before?: number | null;
  limit?: number;
};

/** One inbound row as the history face hands it back. */
function userRow(historyId: number, text = "row") {
  return {
    sender: { id: "u1", kind: "human", handle: "Alice" },
    reply: {
      kind: "user_message",
      history_id: historyId,
      text,
      created_at: "2026-10-22T00:00:00Z",
    },
  };
}

/** One inbound row whose content is empty: it maps to no transcript line. */
function blankRow(historyId: number) {
  return {
    sender: { id: "u1", kind: "human", handle: "Alice" },
    reply: {
      kind: "user_message",
      history_id: historyId,
      text: "   ",
      created_at: "2026-10-22T00:00:00Z",
    },
  };
}

/** A local conversation over a transport whose history face is scripted. */
function conversation(script: (opts: PageRequest) => unknown) {
  const seen: PageRequest[] = [];
  const client = {
    externalEventStream: () => {
      throw new Error("the probe never opens the stream");
    },
    externalHistory: (_id: string, opts: PageRequest) => {
      seen.push(opts);
      const answer = script(opts);
      return answer instanceof Error
        ? Promise.reject(answer)
        : Promise.resolve(answer);
    },
  };
  const transport = new DirectTransport(
    client as never,
    "root",
    LOCAL_OPERATOR_SENDER,
  );
  const conv = new LocalConversation(
    { get: () => undefined } as never,
    transport,
    "conv-local",
  );
  conv.maxRendered = 20;
  return { conv, seen };
}

Deno.test("a probed row resolves from the page just below it", async () => {
  const { conv, seen } = conversation(() => ({
    rows: [userRow(9, "hello")],
    more: true,
  }));
  const line = await conv.peekRow(9);
  if (line === "deleted" || line === "error") {
    throw new Error(`a rendered row resolves, got ${line}`);
  }
  assertEquals(line.historyId, 9);
  assertEquals(line.text, "hello");
  assertEquals(
    seen,
    [{ before: 10, limit: 1 }],
    "one before-page, one row, at the id's own cursor",
  );
});

Deno.test("a contiguous run without the id reads as deleted", async () => {
  const { conv } = conversation(() => ({ rows: [userRow(5)], more: true }));
  assertEquals(await conv.peekRow(9), "deleted");
});

Deno.test("an empty page reads as deleted too", async () => {
  const { conv } = conversation(() => ({ rows: [], more: false }));
  assertEquals(await conv.peekRow(4), "deleted");
});

Deno.test("a failed read stays retryable", async () => {
  const { conv } = conversation(() => new Error("wire broke"));
  assertEquals(await conv.peekRow(9), "error");
});

Deno.test("an id above the rendered head is not probed", async () => {
  const { conv, seen } = conversation(() => ({ rows: [], more: false }));
  assertEquals(await conv.peekRow(21), "error");
  assertEquals(seen, [], "no page is pulled for an unrendered id");
});

Deno.test("a page row that maps to no line stays retryable", async () => {
  const { conv } = conversation(() => ({ rows: [blankRow(9)], more: true }));
  assertEquals(
    await conv.peekRow(9),
    "error",
    "an unreadable row cannot prove the quoted one is gone",
  );
});

Deno.test("a probe reads its page and leaves the window alone", async () => {
  const { conv } = conversation(() => ({
    rows: [userRow(9, "hello")],
    more: true,
  }));
  const lines = conv.transcript.lines.length;
  await conv.peekRow(9);
  assertEquals(conv.transcript.lines.length, lines, "no row is merged");
  assertEquals(conv.maxRendered, 20, "the head does not advance");
  assertEquals(conv.minRendered, 0, "the floor does not move");
});

Deno.test(
  "a stream that is not open answers error without a page",
  async () => {
    const { conv, seen } = conversation(() => ({
      rows: [userRow(9)],
      more: true,
    }));
    conv.status = "reconnecting";
    assertEquals(await conv.peekRow(9), "error");
    assertEquals(seen, [], "no page is pulled while the leaf cannot answer");
  },
);

Deno.test("a page that lost a row stays retryable, not deleted", async () => {
  const { conv } = conversation(() => ({
    rows: [userRow(5)],
    more: true,
    undecodable: 1,
  }));
  assertEquals(
    await conv.peekRow(9),
    "error",
    "a page with a hole cannot prove the target is gone",
  );
});
