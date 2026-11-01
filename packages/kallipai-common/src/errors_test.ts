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

Deno.test(
  "readApiError extracts dangling bindings from the tagma 409",
  async () => {
    const resp = new Response(
      JSON.stringify({
        error: {
          message: "config drops sets still bound by agents: agent-x → 'alt'",
          dangling: ["agent-x → 'alt'"],
        },
      }),
      { status: 409, statusText: "Conflict" },
    );
    const api = await readApiError(resp);
    assertEquals(api.status, 409);
    assertEquals(api.dangling, ["agent-x → 'alt'"]);
  },
);

Deno.test("readApiError omits dangling for errors without it", async () => {
  const resp = new Response(JSON.stringify({ error: { message: "nope" } }), {
    status: 400,
    statusText: "Bad Request",
  });
  const api = await readApiError(resp);
  assertEquals(api.dangling, undefined);
});

Deno.test(
  "readApiError carries the envelope's machine-readable code",
  async () => {
    // The relay separates a retryable authentication-backend outage from a
    // bare 503 that is the offline signal, and it does so by code: an outage
    // answers 503 with `auth_backend_unavailable`, a contract violation 500
    // with `auth_contract_violation`, and only a 503 carrying no code stays
    // a fact about the peer. A surface that later expresses the contract
    // violation as a 503 would keep this parse working without a caller
    // change.
    const resp = new Response(
      JSON.stringify({
        error: {
          message: "archeion unavailable",
          code: "auth_backend_unavailable",
        },
      }),
      { status: 503, statusText: "Service Unavailable" },
    );
    const api = await readApiError(resp);
    assertEquals(api.status, 503);
    assertEquals(api.code, "auth_backend_unavailable");
  },
);

Deno.test("readApiError leaves code empty on a non-JSON body", async () => {
  // A reverse proxy's plain-text 503, or the CSRF guard's 403: there is no
  // envelope to parse, so no code rides along.
  const resp = new Response("upstream connect error", { status: 503 });
  const api = await readApiError(resp);
  assertEquals(api.message, "upstream connect error");
  assertEquals(api.code, undefined);
});
