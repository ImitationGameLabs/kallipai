// Tests for DirectTransport.send's parked-409 auto-wake: the direct path's
// answer to "agent is parked; use POST /agents/{id}/wake" — wake, back off,
// re-send; never silently drop, never double-send (the 409 fires before the
// tagma's inbox push, so an unsent message never landed).

import { assertEquals, assertRejects } from "@std/assert";
import { KallipError } from "@kallipai/kallip-common";
import type { TagmaClient } from "@kallipai/kallip-client";
import { DirectTransport } from "./directTransport.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

const PARKED_409 = () =>
  new KallipError({
    status: 409,
    message: "agent is parked; use POST /agents/{id}/wake to kick it awake",
  });

/** A TagmaClient whose postMessage walks a scripted outcome list ("ok"
 * resolves, an Error instance rejects with it), counting calls; wakeAgent
 * optionally rejects. The SSE stream stays empty — these tests exercise
 * send only. */
function sendClient(
  outcomes: ("ok" | Error)[],
  wakeFails = false,
): { client: TagmaClient; state: { wakeCalls: number; posts: number } } {
  const state = { wakeCalls: 0, posts: 0 };
  const client = {
    wakeAgent() {
      state.wakeCalls++;
      return wakeFails
        ? Promise.reject(new Error("wake refused"))
        : Promise.resolve();
    },
    async postMessage(_id: string, _text: string): Promise<void> {
      state.posts++;
      const next = outcomes.shift();
      if (next instanceof Error) throw next;
    },
    async *externalEventStream() {},
  } as unknown as TagmaClient;
  return { client, state };
}

Deno.test("send auto-wakes on a parked 409 and re-sends", async () => {
  const { client, state } = sendClient([PARKED_409(), "ok"]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  await t.send("hello");
  assertEquals(state.posts, 2); // first attempt rejected, retry delivered
  assertEquals(state.wakeCalls, 1);
});

Deno.test("a failed wake call does not abort the retry loop", async () => {
  const { client, state } = sendClient([PARKED_409(), "ok"], true);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  await t.send("hello");
  assertEquals(state.posts, 2);
});

Deno.test("send exhausts retries and rethrows the original 409", async () => {
  const { client, state } = sendClient([
    PARKED_409(),
    PARKED_409(),
    PARKED_409(),
  ]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1, 1]);
  const err = await assertRejects(() => t.send("hello"), KallipError);
  assertEquals(err.api.status, 409);
  assertEquals(state.posts, 3); // every attempt hit the parked guard
  assertEquals(state.wakeCalls, 1); // one wake attempt, not one per retry
});

Deno.test("send propagates non-parked errors without waking", async () => {
  const { client, state } = sendClient([new Error("network down")]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  await assertRejects(() => t.send("hello"), Error, "network down");
  assertEquals(state.posts, 1);
  assertEquals(state.wakeCalls, 0);
});

Deno.test("a mid-retry non-parked error beats the parked 409", async () => {
  const gone = new KallipError({ status: 404, message: "agent not found" });
  const { client } = sendClient([PARKED_409(), gone]);
  const t = new DirectTransport(client, "root", LOCAL_OPERATOR_SENDER, [1]);
  const err = await assertRejects(() => t.send("hello"), KallipError);
  assertEquals(err.api.status, 404);
});
