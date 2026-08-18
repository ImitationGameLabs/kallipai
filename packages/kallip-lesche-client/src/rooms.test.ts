import { assertEquals } from "@std/assert";
import { assertRejects } from "@std/assert";
import type { Envelope, RoomMessageView, RoomRosterView } from "./types.ts";
import { LescheApiError } from "./types.ts";
import { LescheClient } from "./http.ts";

// HTTP-client contract tests for the rooms surface. The data-plane routes
// (`/v1/rooms/{id}/envelopes`, `.../messages`) come first; the management
// routes (`/v1/rooms`, `.../invites`, `.../participants`, `.../tagmata`, roster)
// follow. Same shape throughout: stub globalThis.fetch, pin the emitted request
// (path, method, CSRF marker, JSON body / query) + the decoded response. A
// wire-shape drift (a renamed field, a wrong method, a missing CSRF marker, a
// malformed query) fails here, not at runtime.

const BASE = "https://lesche.test";

/** Swap globalThis.fetch for the test, restore it after. */
function withFetch(
  fetchImpl: typeof fetch,
  fn: () => Promise<void>,
): Promise<void> {
  const original = globalThis.fetch;
  globalThis.fetch = fetchImpl;
  return fn().finally(() => {
    globalThis.fetch = original;
  });
}

const ENVELOPE: Envelope = {
  channel_id: "room-1",
  sender: { id: "p-alice", kind: "human", handle: "Alice" },
  sequence_n: 3,
  trace_id: "tr",
  timestamp: "2026-08-03T00:00:00Z",
  ciphertext: "AAAA",
};

Deno.test("postRoomEnvelope POSTs /v1/rooms/{id}/envelopes", async () => {
  const captured: {
    url: string;
    method: string;
    headers: Record<string, string>;
    body: unknown;
  }[] = [];
  const stub: typeof fetch = (input, init) => {
    captured.push({
      url: typeof input === "string" ? input : input.toString(),
      method: init?.method ?? "GET",
      headers: (init?.headers as Record<string, string>) ?? {},
      body: init?.body ? JSON.parse(init.body as string) : null,
    });
    return Promise.resolve(
      new Response(null, { status: 202, headers: { "content-length": "0" } }),
    );
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    await c.postRoomEnvelope("room-1", ENVELOPE);
  });
  assertEquals(
    captured[0]!.url,
    "https://lesche.test/v1/rooms/room-1/envelopes",
  );
  assertEquals(captured[0]!.method, "POST");
  assertEquals(captured[0]!.headers["X-Requested-With"], "kallip");
  assertEquals(captured[0]!.body, ENVELOPE);
});

Deno.test(
  "fetchRoomMessages builds the after_seq/limit query + decodes",
  async () => {
    const captured: { url: string; method: string }[] = [];
    const stub: typeof fetch = (input, init) => {
      captured.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
      });
      return Promise.resolve(
        new Response(
          JSON.stringify([
            {
              seq: 4,
              sender: { id: "p-t1", kind: "agent", handle: "Tagma" },
              epoch: 1,
              ciphertext: "AAAA",
              created_at: "2026-08-03T00:00:00Z",
            },
          ]),
          { status: 200, headers: { "content-type": "application/json" } },
        ),
      );
    };
    let out: RoomMessageView[];
    await withFetch(stub, async () => {
      const c = new LescheClient(BASE);
      out = await c.fetchRoomMessages("room-1", { afterSeq: 3, limit: 50 });
    });
    assertEquals(
      captured[0]!.url,
      "https://lesche.test/v1/rooms/room-1/messages?after_seq=3&limit=50",
    );
    assertEquals(captured[0]!.method, "GET");
    assertEquals(out!, [
      {
        seq: 4,
        sender: { id: "p-t1", kind: "agent", handle: "Tagma" },
        epoch: 1,
        ciphertext: "AAAA",
        created_at: "2026-08-03T00:00:00Z",
      },
    ]);
  },
);

Deno.test("fetchRoomMessages omits the query when no opts given", async () => {
  const captured: { url: string }[] = [];
  const stub: typeof fetch = (input) => {
    captured.push({
      url: typeof input === "string" ? input : input.toString(),
    });
    return Promise.resolve(new Response("[]", { status: 200 }));
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    await c.fetchRoomMessages("room-1");
  });
  assertEquals(
    captured[0]!.url,
    "https://lesche.test/v1/rooms/room-1/messages",
  );
});

// -- management surface (create / list / invite / members / tagmata / roster) --

Deno.test("createRoom POSTs /v1/rooms and decodes RoomView", async () => {
  const captured: {
    url: string;
    method: string;
    headers: Record<string, string>;
    body?: string;
  }[] = [];
  const stub: typeof fetch = (input, init) => {
    captured.push({
      url: typeof input === "string" ? input : input.toString(),
      method: init?.method ?? "GET",
      headers: (init?.headers as Record<string, string>) ?? {},
      body: init?.body?.toString(),
    });
    return Promise.resolve(
      new Response(
        JSON.stringify({
          room_id: "room-1",
          created_at: "2026-08-03T00:00:00Z",
          name: "General",
          description: "",
          visibility: "private",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    const out = await c.createRoom({ name: "General" });
    assertEquals(out, {
      room_id: "room-1",
      created_at: "2026-08-03T00:00:00Z",
      name: "General",
      description: "",
      visibility: "private",
    });
  });
  assertEquals(captured[0]!.url, "https://lesche.test/v1/rooms");
  assertEquals(captured[0]!.method, "POST");
  assertEquals(captured[0]!.headers["X-Requested-With"], "kallip");
  // All three fields are always sent (name required; description + visibility
  // defaulted) so the server's Json extractor does not reject the body.
  assertEquals(
    captured[0]!.body,
    JSON.stringify({ name: "General", description: "", visibility: "private" }),
  );
});

Deno.test("createRoom(public) sends the visibility body", async () => {
  const captured: { body?: string }[] = [];
  const stub: typeof fetch = (_input, init) => {
    captured.push({ body: init?.body?.toString() });
    return Promise.resolve(
      new Response(
        JSON.stringify({
          room_id: "room-pub",
          created_at: "2026-08-03T00:00:00Z",
          name: "Town square",
          description: "",
          visibility: "public",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
  };
  await withFetch(stub, async () => {
    const out = await new LescheClient(BASE).createRoom({
      name: "Town square",
      visibility: "public",
    });
    assertEquals(out.visibility, "public");
  });
  assertEquals(
    captured[0]!.body,
    JSON.stringify({
      name: "Town square",
      description: "",
      visibility: "public",
    }),
  );
});

Deno.test("listPublicRooms GETs /v1/rooms/public", async () => {
  const captured: string[] = [];
  const stub: typeof fetch = (input) => {
    captured.push(typeof input === "string" ? input : input.toString());
    return Promise.resolve(
      new Response(
        JSON.stringify([
          {
            room_id: "rp",
            created_at: "2026-08-03T00:00:00Z",
            visibility: "public",
          },
        ]),
        { status: 200 },
      ),
    );
  };
  await withFetch(stub, async () => {
    const out = await new LescheClient(BASE).listPublicRooms();
    assertEquals(out.length, 1);
    assertEquals(out[0]!.visibility, "public");
  });
  assertEquals(captured[0], "https://lesche.test/v1/rooms/public");
});

Deno.test("joinRoom POSTs /v1/rooms/{id}/join", async () => {
  const captured: { url: string; method: string }[] = [];
  const stub: typeof fetch = (input, init) => {
    captured.push({
      url: typeof input === "string" ? input : input.toString(),
      method: init?.method ?? "GET",
    });
    return Promise.resolve(new Response(null, { status: 204 }));
  };
  await withFetch(stub, async () => {
    await new LescheClient(BASE).joinRoom("room-pub");
  });
  assertEquals(captured[0]!.url, "https://lesche.test/v1/rooms/room-pub/join");
  assertEquals(captured[0]!.method, "POST");
});

Deno.test("listRooms GETs /v1/rooms with no CSRF marker", async () => {
  const captured: string[] = [];
  const stub: typeof fetch = (input) => {
    captured.push(
      typeof input === "string" ? input : (input as URL).toString(),
    );
    return Promise.resolve(
      new Response(
        JSON.stringify([{ room_id: "r", created_at: "2026-01-01T00:00:00Z" }]),
        { status: 200 },
      ),
    );
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    const out = await c.listRooms();
    assertEquals(out.length, 1);
    assertEquals(out[0]!.room_id, "r");
  });
  assertEquals(captured, ["https://lesche.test/v1/rooms"]);
});

Deno.test("listMyRoomInvites GETs /v1/rooms/invites", async () => {
  const captured: string[] = [];
  const stub: typeof fetch = (input) => {
    captured.push(typeof input === "string" ? input : input.toString());
    return Promise.resolve(
      new Response(
        JSON.stringify([
          {
            invite_id: "inv-1",
            room_id: "r",
            invited_by: "@u",
            created_at: "2026-01-01T00:00:00Z",
            expires_at: "2026-01-08T00:00:00Z",
          },
        ]),
        { status: 200 },
      ),
    );
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    const out = await c.listMyRoomInvites();
    assertEquals(out[0]!.invite_id, "inv-1");
  });
  assertEquals(captured, ["https://lesche.test/v1/rooms/invites"]);
});

Deno.test(
  "createRoomInvite POSTs the invitee username and decodes the response",
  async () => {
    const captured: { url: string; method: string; body?: string }[] = [];
    const stub: typeof fetch = (input, init) => {
      captured.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
        body: typeof init?.body === "string" ? init.body : undefined,
      });
      return Promise.resolve(
        new Response(
          JSON.stringify({
            invite_id: "inv-1",
            expires_at: "2026-01-08T00:00:00Z",
          }),
          { status: 201 },
        ),
      );
    };
    await withFetch(stub, async () => {
      const c = new LescheClient(BASE);
      const out = await c.createRoomInvite("room-1", "user-2");
      assertEquals(out.invite_id, "inv-1");
    });
    assertEquals(
      captured[0]!.url,
      "https://lesche.test/v1/rooms/room-1/invites",
    );
    assertEquals(captured[0]!.method, "POST");
    assertEquals(
      captured[0]!.body,
      JSON.stringify({ invitee_username: "user-2" }),
    );
  },
);

Deno.test(
  "acceptRoomInvite POSTs the accept path and resolves void on 204",
  async () => {
    const captured: { url: string; method: string }[] = [];
    const stub: typeof fetch = (input, init) => {
      captured.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
      });
      return Promise.resolve(new Response(null, { status: 204 }));
    };
    await withFetch(stub, async () => {
      const c = new LescheClient(BASE);
      const out = await c.acceptRoomInvite("room-1", "inv-1");
      assertEquals(out, undefined);
    });
    assertEquals(captured[0]!, {
      url: "https://lesche.test/v1/rooms/room-1/invites/inv-1/accept",
      method: "POST",
    });
  },
);

Deno.test(
  "removeRoomMember DELETEs the member path with the member id",
  async () => {
    const captured: { url: string; method: string }[] = [];
    const stub: typeof fetch = (input, init) => {
      captured.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
      });
      return Promise.resolve(new Response(null, { status: 204 }));
    };
    await withFetch(stub, async () => {
      const c = new LescheClient(BASE);
      await c.removeRoomMember("room-1", "mid-1");
    });
    assertEquals(captured[0]!, {
      url: "https://lesche.test/v1/rooms/room-1/members/mid-1",
      method: "DELETE",
    });
  },
);

Deno.test("addRoomTagma POSTs the tagma id body", async () => {
  const captured: { body?: string }[] = [];
  const stub: typeof fetch = (_input, init) => {
    captured.push({
      body: typeof init?.body === "string" ? init.body : undefined,
    });
    return Promise.resolve(new Response(null, { status: 204 }));
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    await c.addRoomTagma("room-1", "tagma-9");
  });
  assertEquals(captured[0]!.body, JSON.stringify({ tagma_id: "tagma-9" }));
});

Deno.test("listMyTagmaRooms GETs the owner tagma-rooms path", async () => {
  const captured: { url: string; method: string }[] = [];
  const stub: typeof fetch = (input, init) => {
    captured.push({
      url: typeof input === "string" ? input : input.toString(),
      method: init?.method ?? "GET",
    });
    return Promise.resolve(
      new Response(
        JSON.stringify([
          {
            room_id: "room-1",
            members: [{ id: "pid-1", kind: "agent" }],
            membership_epoch: 3,
            is_creator: true,
            visibility: "private",
            name: "Room one",
          },
          // `name` is optional on the wire (omitted when the room has none).
          {
            room_id: "room-2",
            members: [{ id: "pid-1", kind: "agent" }],
            membership_epoch: 1,
            is_creator: false,
            visibility: "public",
          },
        ]),
        { status: 200 },
      ),
    );
  };
  await withFetch(stub, async () => {
    const c = new LescheClient(BASE);
    const out = await c.listMyTagmaRooms("tagma-9");
    assertEquals(out.length, 2);
    assertEquals(out[0]!.room_id, "room-1");
    assertEquals(out[0]!.name, "Room one");
    assertEquals(out[1]!.name, undefined);
  });
  assertEquals(captured[0]!, {
    url: "https://lesche.test/v1/me/tagmata/tagma-9/rooms",
    method: "GET",
  });
});

Deno.test(
  "a non-2xx response throws LescheApiError with the server message",
  async () => {
    const stub: typeof fetch = () =>
      Promise.resolve(
        new Response(
          JSON.stringify({ error: { message: "invite already pending" } }),
          {
            status: 409,
          },
        ),
      );
    await withFetch(stub, async () => {
      const c = new LescheClient(BASE);
      const err = await assertRejects(
        () => c.createRoomInvite("room-1", "user-2"),
        LescheApiError,
      );
      assertEquals(err.status, 409);
      assertEquals(err.message, "invite already pending");
    });
  },
);

Deno.test(
  "fetchRoomRoster GETs /v1/rooms/{id} and decodes the roster",
  async () => {
    const captured: { url: string; method: string }[] = [];
    const stub: typeof fetch = (input, init) => {
      captured.push({
        url: typeof input === "string" ? input : input.toString(),
        method: init?.method ?? "GET",
      });
      return Promise.resolve(
        new Response(
          JSON.stringify({
            room_id: "room-1",
            members: [
              { id: "p-alice", kind: "human", handle: "@alice", online: true },
              { id: "p-bob", kind: "human", handle: "@bob", online: false },
              {
                id: "p-t1",
                kind: "agent",
                handle: "p-t1@alice",
                label: "Helper",
                online: true,
              },
            ],
            membership_epoch: 3,
            is_creator: true,
            visibility: "private",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        ),
      );
    };
    let out: RoomRosterView;
    await withFetch(stub, async () => {
      out = await new LescheClient(BASE).fetchRoomRoster("room-1");
    });
    assertEquals(captured[0]!.url, "https://lesche.test/v1/rooms/room-1");
    assertEquals(captured[0]!.method, "GET");
    assertEquals(out!, {
      room_id: "room-1",
      members: [
        { id: "p-alice", kind: "human", handle: "@alice", online: true },
        { id: "p-bob", kind: "human", handle: "@bob", online: false },
        {
          id: "p-t1",
          kind: "agent",
          handle: "p-t1@alice",
          label: "Helper",
          online: true,
        },
      ],
      membership_epoch: 3,
      is_creator: true,
      visibility: "private",
    });
  },
);
