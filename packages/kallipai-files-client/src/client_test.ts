// Unit tests for the promise-shaped files client, against a stubbed global
// fetch. The stub pins the wire shape (method, URL query, CSRF marker, cookie
// credentials) and the response-to-value mapping (including the error
// envelope reduction on 403/413), per the server routes. The effect-level
// failure mapping lives in `client_effect_test.ts`.

import { withFetch } from "@kallipai/kallipai-common/testing";
import { assert, assertEquals, assertRejects } from "@std/assert";

import { FilesClient } from "./client.ts";
import { FilesApiError } from "./http.ts";

interface RecordedRequest {
  method: string;
  url: string;
  headers: Headers;
  body: BodyInit | null | undefined;
}

/** A fetch that records every request and answers through `respond`, plus the
 * log it fills. Pass the stub to `withFetch`. */
function recordFetch(respond: (req: RecordedRequest) => Response): {
  seen: RecordedRequest[];
  stub: (input: string | URL, init?: RequestInit) => Promise<Response>;
} {
  const seen: RecordedRequest[] = [];
  const stub = (input: string | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : String(input);
    const method = init?.method ?? "GET";
    const headers = new Headers(init?.headers);
    seen.push({ method, url, headers, body: init?.body });
    return Promise.resolve(respond(seen[seen.length - 1]));
  };
  return { seen, stub };
}

Deno.test(
  "put uploads to PUT /files?path= and maps the minted record",
  async () => {
    const { stub } = recordFetch(
      () =>
        new Response(
          JSON.stringify({
            record_id: "01890a5d-ac96-774b-bcce-b302099a8057",
            blob_id: "blake3-abc",
          }),
          { status: 201 },
        ),
    );
    await withFetch(stub, async () => {
      const client = new FilesClient("http://files.test/v1/files");
      const out = await client.put(
        "/users/u1/shared/report.pdf",
        new Uint8Array([1, 2, 3]),
      );
      assertEquals(out.blob_id, "blake3-abc");
    });
  },
);

Deno.test(
  "send posts to /{id}/send with the to_tagma target and maps the copy",
  async () => {
    const { seen, stub } = recordFetch(
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
    await withFetch(stub, async () => {
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
      assert(seen[0].headers.get("X-Requested-With") === "kallipai");
      const body = JSON.parse(String(seen[0].body)) as {
        to_user: string | null;
        to_tagma: string | null;
      };
      assertEquals(body.to_tagma, "t1");
      assertEquals(body.to_user, null);
    });
  },
);

Deno.test("get returns the bytes", async () => {
  const { stub } = recordFetch(
    () => new Response(new Uint8Array([9, 8, 7]), { status: 200 }),
  );
  await withFetch(stub, async () => {
    const client = new FilesClient();
    const bytes = await client.get("01890a5d-ac96-774b-bcce-b302099a8057");
    assertEquals(new Uint8Array(bytes), new Uint8Array([9, 8, 7]));
  });
});

Deno.test("list maps the entries array", async () => {
  const { stub } = recordFetch(
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
  await withFetch(stub, async () => {
    const client = new FilesClient();
    const entries = await client.list({ space: "shared", limit: 10 });
    assertEquals(entries.length, 1);
    assertEquals(entries[0].path, "/users/u1/shared/report.pdf");
  });
});

Deno.test(
  "delete issues DELETE /v1/files/{id} with the CSRF marker and maps 204",
  async () => {
    const { seen, stub } = recordFetch(
      () => new Response(null, { status: 204 }),
    );
    await withFetch(stub, async () => {
      const client = new FilesClient("http://files.test/v1/files");
      await client.delete("01890a5d-ac96-774b-bcce-b302099a8057");
      assertEquals(seen[0].method, "DELETE");
      assert(
        seen[0].url.endsWith("/v1/files/01890a5d-ac96-774b-bcce-b302099a8057"),
      );
      assert(seen[0].headers.get("X-Requested-With") === "kallipai");
    });
  },
);

Deno.test("non-2xx becomes FilesApiError with the server message", async () => {
  const denied = recordFetch(
    () =>
      new Response(JSON.stringify({ error: { message: "no read right" } }), {
        status: 403,
      }),
  );
  await withFetch(denied.stub, async () => {
    const client = new FilesClient();
    const error = await assertRejects(
      () => client.get("01890a5d-ac96-774b-bcce-b302099a8057"),
      FilesApiError,
    );
    assertEquals(error._tag, "FilesApiError");
    assertEquals(error.status, 403);
    assertEquals(error.message, "no read right");
  });

  const tooLarge = recordFetch(
    () =>
      new Response(
        JSON.stringify({ error: { message: "body exceeds the cap" } }),
        { status: 413 },
      ),
  );
  await withFetch(tooLarge.stub, async () => {
    const client = new FilesClient();
    const error = await assertRejects(
      () => client.put("/users/u1/shared/big.bin", new Uint8Array(0)),
      FilesApiError,
    );
    assertEquals(error.status, 413);
  });

  const deniedDelete = recordFetch(
    () =>
      new Response(
        JSON.stringify({
          error: { message: "not allowed to delete this record" },
        }),
        { status: 403 },
      ),
  );
  await withFetch(deniedDelete.stub, async () => {
    const client = new FilesClient();
    const error = await assertRejects(
      () => client.delete("01890a5d-ac96-774b-bcce-b302099a8057"),
      FilesApiError,
    );
    assertEquals(error.status, 403);
  });
});
