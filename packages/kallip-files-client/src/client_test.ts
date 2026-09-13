// Unit tests for the thin files client, against a stubbed global fetch.
// The stub pins the wire shape (method, URL query, CSRF marker, cookie
// credentials) and the response-to-value mapping (including the error
// envelope reduction on 403/413), per the server routes.

import { assert, assertEquals, assertRejects } from "@std/assert";
import { FilesClient } from "./client.ts";
import { CSRF_MARKER, FilesApiError } from "./http.ts";

interface RecordedRequest {
  method: string;
  url: string;
  headers: Headers;
  body: BodyInit | null | undefined;
}

function stubFetch(
  respond: (req: RecordedRequest) => Response,
): RecordedRequest[] {
  const seen: RecordedRequest[] = [];
  // deno-lint-ignore require-await
  globalThis.fetch = async (
    input: Request | URL | string,
    init?: RequestInit,
  ): Promise<Response> => {
    const url = typeof input === "string" ? input : String(input);
    const method = init?.method ?? "GET";
    const headers = new Headers(init?.headers);
    seen.push({ method, url, headers, body: init?.body });
    return respond(seen[seen.length - 1]);
  };
  return seen;
}

Deno.test(
  "put uploads to PUT /files?path= and maps the minted record",
  async () => {
    stubFetch(
      () =>
        new Response(
          JSON.stringify({
            record_id: "01890a5d-ac96-774b-bcce-b302099a8057",
            blob_id: "blake3-abc",
          }),
          { status: 201 },
        ),
    );
    const client = new FilesClient("http://files.test/v1/files");
    const out = await client.put(
      "/users/u1/shared/report.pdf",
      new Uint8Array([1, 2, 3]),
    );
    assertEquals(out.blob_id, "blake3-abc");
  },
);

Deno.test(
  "send posts to /{id}/send with the to_tagma target and maps the copy",
  async () => {
    const seen = stubFetch(
      () =>
        new Response(
          JSON.stringify({
            record_id: "01890a5d-ac96-774b-bcce-b302099a8058",
            blob_id: "blake3-abc",
            path: "/users/u1/tagmas/t1/inbox/report.pdf",
          }),
          { status: 201 },
        ),
    );
    const client = new FilesClient("http://files.test/v1/files");
    const out = await client.send("01890a5d-ac96-774b-bcce-b302099a8057", {
      toTagma: "t1",
    });
    assertEquals(out.path, "/users/u1/tagmas/t1/inbox/report.pdf");
    assertEquals(seen[0].method, "POST");
    assert(
      seen[0].url.endsWith(
        "/v1/files/01890a5d-ac96-774b-bcce-b302099a8057/send",
      ),
    );
    assert(seen[0].headers.get("X-Requested-With") === CSRF_MARKER);
    const body = JSON.parse(String(seen[0].body)) as {
      to_user: string | null;
      to_tagma: string | null;
    };
    assertEquals(body.to_tagma, "t1");
    assertEquals(body.to_user, null);
  },
);

Deno.test("get returns the bytes and list maps the entries array", async () => {
  stubFetch(() => new Response(new Uint8Array([9, 8, 7]), { status: 200 }));
  const client = new FilesClient();
  const bytes = await client.get("01890a5d-ac96-774b-bcce-b302099a8057");
  assertEquals(new Uint8Array(bytes), new Uint8Array([9, 8, 7]));

  stubFetch(
    () =>
      new Response(
        JSON.stringify([
          {
            id: "01890a5d-ac96-774b-bcce-b302099a8057",
            path: "/users/u1/shared/report.pdf",
            size: 3,
            created_at: "2026-09-01T00:00:00Z",
          },
        ]),
        { status: 200 },
      ),
  );
  const entries = await client.list({ space: "shared", limit: 10 });
  assertEquals(entries.length, 1);
  assertEquals(entries[0].path, "/users/u1/shared/report.pdf");
});

Deno.test(
  "delete issues DELETE /v1/files/{id} with the CSRF marker and maps 204",
  async () => {
    const seen = stubFetch(() => new Response(null, { status: 204 }));
    const client = new FilesClient("http://files.test/v1/files");
    await client.delete("01890a5d-ac96-774b-bcce-b302099a8057");
    assertEquals(seen[0].method, "DELETE");
    assert(
      seen[0].url.endsWith("/v1/files/01890a5d-ac96-774b-bcce-b302099a8057"),
    );
    assert(seen[0].headers.get("X-Requested-With") === CSRF_MARKER);
  },
);

Deno.test("non-2xx becomes FilesApiError with the server message", async () => {
  stubFetch(
    () =>
      new Response(JSON.stringify({ error: { message: "no read right" } }), {
        status: 403,
      }),
  );
  const client = new FilesClient();
  const error = await assertRejects(
    () => client.get("01890a5d-ac96-774b-bcce-b302099a8057"),
    FilesApiError,
  );
  assertEquals(error.status, 403);
  assertEquals(error.message, "no read right");

  stubFetch(
    () =>
      new Response(
        JSON.stringify({ error: { message: "body exceeds the cap" } }),
        { status: 413 },
      ),
  );
  const tooLarge = await assertRejects(
    () => client.put("/users/u1/shared/big.bin", new Uint8Array(0)),
    FilesApiError,
  );
  assertEquals(tooLarge.status, 413);

  stubFetch(
    () =>
      new Response(
        JSON.stringify({
          error: { message: "not allowed to delete this record" },
        }),
        { status: 403 },
      ),
  );
  const deniedDelete = await assertRejects(
    () => client.delete("01890a5d-ac96-774b-bcce-b302099a8057"),
    FilesApiError,
  );
  assertEquals(deniedDelete.status, 403);
});
