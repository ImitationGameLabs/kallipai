import { assertEquals } from "@std/assert";
import { parseSseStream } from "./sse.ts";

function sseResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks) controller.enqueue(encoder.encode(c));
      controller.close();
    },
  });
  return new Response(stream);
}

async function collect(chunks: string[]): Promise<string[]> {
  const out: string[] = [];
  for await (const ev of parseSseStream(sseResponse(chunks))) {
    out.push(ev.data);
  }
  return out;
}

Deno.test("parses two events delivered in one chunk", async () => {
  const data = await collect(['data: {"a":1}\n\ndata: {"a":2}\n\n']);
  assertEquals(data, ['{"a":1}', '{"a":2}']);
});

Deno.test("reassembles a data line split across chunks", async () => {
  const data = await collect(["data: hello", "-world\n\n"]);
  assertEquals(data, ["hello-world"]);
});

Deno.test("joins multiple data: lines with newline", async () => {
  const data = await collect(["data: line1\ndata: line2\n\n"]);
  assertEquals(data, ["line1\nline2"]);
});

Deno.test("ignores comment / keepalive lines", async () => {
  const data = await collect([": keepalive\n\ndata: real\n\n"]);
  assertEquals(data, ["real"]);
});

Deno.test("yields a trailing event without a final blank line", async () => {
  const data = await collect(["data: tail"]);
  assertEquals(data, ["tail"]);
});
