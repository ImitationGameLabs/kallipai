// The local leaf's window sentinel: the catch-up batch and the scroll-up page
// both decide whether older history exists. A row the tagma could not decode
// makes a page short without exhausting the history, while a page with no
// renderable row at all cannot move the cursor and must not be re-requested --
// two different answers these cases pin apart.
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

/** One history page as the external face hands it back. */
type Page = {
  rows: ReturnType<typeof userRow>[];
  more: boolean;
  undecodable: number;
};

/** One inbound row as the history face hands it back. */
function userRow(historyId: number) {
  return {
    sender: { id: "u1", kind: "human", handle: "Alice" },
    reply: {
      kind: "user_message",
      history_id: historyId,
      text: `row ${historyId}`,
      created_at: "2026-10-22T00:00:00Z",
    },
  };
}

/** A local conversation over a transport whose history face is scripted. */
function conversation(script: () => Page) {
  const seen: PageRequest[] = [];
  const client = {
    externalEventStream: () => {
      throw new Error("these cases never open the stream");
    },
    externalHistory: (_id: string, opts: PageRequest) => {
      seen.push(opts);
      return Promise.resolve(script());
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
  return { conv, seen };
}

Deno.test(
  "a recent batch short by a hole keeps the sentinel armed",
  async () => {
    // Two rows rendered and one dropped, against a page of three: the batch is
    // short only because of the hole, so older history may still be there.
    const { conv } = conversation(() => ({
      rows: [userRow(19), userRow(20)],
      more: false,
      undecodable: 1,
    }));
    await conv.catchUp(3);
    assertEquals(conv.transcript.lines.length, 2);
    assertEquals(conv.hasMoreOlder, true);
  },
);

Deno.test(
  "a recent batch short without a hole disarms the sentinel",
  async () => {
    // The control the case above needs: three raw rows are not there to be had.
    const { conv } = conversation(() => ({
      rows: [userRow(19), userRow(20)],
      more: false,
      undecodable: 0,
    }));
    await conv.catchUp(3);
    assertEquals(conv.hasMoreOlder, false);
  },
);

Deno.test(
  "an all-dropped before-page disarms and is not re-paged",
  async () => {
    // Nothing in this page mapped to a line, so the cursor stays where it was:
    // paging again would return the same rows. The verdict is about this window
    // being unpageable, not about the history ending.
    const { conv, seen } = conversation(() => ({
      rows: [],
      more: true,
      undecodable: 3,
    }));
    conv.minRendered = 20;
    conv.maxRendered = 20;
    await conv.loadOlder(3);
    assertEquals(conv.hasMoreOlder, false);
    await conv.loadOlder(3);
    assertEquals(seen.length, 1, "a disarmed sentinel pages nothing");
  },
);

Deno.test(
  "a before-page that reports no more disarms the sentinel",
  async () => {
    // The other arm: the tagma itself says nothing older exists.
    const { conv } = conversation(() => ({
      rows: [userRow(19)],
      more: false,
      undecodable: 0,
    }));
    conv.minRendered = 20;
    conv.maxRendered = 20;
    await conv.loadOlder(3);
    assertEquals(conv.hasMoreOlder, false);
  },
);
