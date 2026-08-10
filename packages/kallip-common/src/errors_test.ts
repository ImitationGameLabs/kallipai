import { assertEquals } from "@std/assert";
import { readApiError } from "./errors.ts";

Deno.test("readApiError parses the nested error envelope", async () => {
  const resp = new Response(
    JSON.stringify({ error: { message: "room is full" } }),
    { status: 409, statusText: "Conflict" },
  );
  const api = await readApiError(resp);
  assertEquals(api.status, 409);
  assertEquals(api.message, "room is full");
});

Deno.test(
  "readApiError returns a non-envelope JSON body verbatim",
  async () => {
    // Valid JSON that is NOT the {"error":{"message":...}} shape (e.g. the flat
    // body the old drifted parsers read): no field to extract, so the raw body is
    // more informative than statusText.
    const resp = new Response(JSON.stringify({ message: "room is full" }), {
      status: 409,
      statusText: "Conflict",
    });
    const api = await readApiError(resp);
    assertEquals(api.status, 409);
    assertEquals(api.message, JSON.stringify({ message: "room is full" }));
  },
);

Deno.test("readApiError surfaces a non-JSON text body", async () => {
  const resp = new Response("missing CSRF marker", { status: 403 });
  const api = await readApiError(resp);
  assertEquals(api.status, 403);
  assertEquals(api.message, "missing CSRF marker");
});

Deno.test(
  "readApiError falls back to statusText for an empty body",
  async () => {
    const resp = new Response("", { status: 502, statusText: "Bad Gateway" });
    const api = await readApiError(resp);
    assertEquals(api.status, 502);
    assertEquals(api.message, "Bad Gateway");
  },
);
