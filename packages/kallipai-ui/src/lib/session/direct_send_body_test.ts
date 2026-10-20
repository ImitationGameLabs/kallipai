// The direct send's wire body, asserted where the bytes leave the process: a
// stubbed fetch records the JSON the real TagmaClient posts, so a body that
// lost a key fails here (a client-level mock would pass regardless). Three
// shapes are pinned: quote only, neither optional piece (byte-identical to the
// historical body), and both together.

import { assertEquals } from "@std/assert";
import type { FileAttachment } from "@kallipai/kallipai-lesche-client";
import { TagmaClient } from "@kallipai/kallipai-client";
import { DirectTransport } from "./directTransport.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

/** Swap fetch for a recorder that answers every POST with a 202 receipt. */
function recordBodies(): { bodies: string[]; restore: () => void } {
  const bodies: string[] = [];
  const real = globalThis.fetch;
  globalThis.fetch = ((_input: unknown, init?: RequestInit) => {
    bodies.push(String(init?.body ?? ""));
    return Promise.resolve(
      new Response(JSON.stringify({ queue_depth: 0 }), {
        status: 202,
        headers: { "content-type": "application/json" },
      }),
    );
  }) as typeof fetch;
  return { bodies, restore: () => (globalThis.fetch = real) };
}

async function bodyOf(options?: {
  attachment?: FileAttachment;
  quote?: number;
}): Promise<string> {
  const { bodies, restore } = recordBodies();
  try {
    const transport = new DirectTransport(
      new TagmaClient({ baseUrl: "http://tagma.test", authToken: "t" }),
      "root",
      LOCAL_OPERATOR_SENDER,
    );
    try {
      await transport.send("x", options);
    } finally {
      transport.close();
    }
    assertEquals(bodies.length, 1, "one POST per send");
    return bodies[0]!;
  } finally {
    restore();
  }
}

Deno.test("a quoted direct send carries the row id", async () => {
  assertEquals(await bodyOf({ quote: 7 }), '{"text":"x","quote":7}');
});

Deno.test(
  "a direct send with no payload keeps the historical body",
  async () => {
    assertEquals(await bodyOf(), '{"text":"x"}');
  },
);

Deno.test("attachment and quote ride one body together", async () => {
  const attachment = {
    record_id: "0f8fad5b-d9cb-469f-a165-70867728950e",
    name: "a.txt",
    size: 3,
  } as FileAttachment;
  assertEquals(
    await bodyOf({ attachment, quote: 7 }),
    `{"text":"x","attachment":${JSON.stringify(attachment)},"quote":7}`,
  );
});
