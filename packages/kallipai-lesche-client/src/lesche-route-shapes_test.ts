// Two-end reconciliation: every URL the package's client dials must exist
// in the shared route fixture the rust shape test also asserts against
// (crates/platform/kallipai-lesche/src/route-shapes.json). A one-sided URL
// change fails a test on both ends. The manage/projection dials live
// on the keryx client; this file pins the rooms family this client owns.
// Query strings are outside the pin: the stub strips them before
// matching, so a query drift is not caught here. The direct-sessions
// family (five rows) has no TS methods yet; the two tagma-self rows
// are dialed from Rust.

import { assertEquals } from "@std/assert";
import shapes from "../../../crates/platform/kallipai-lesche/src/route-shapes.json" with { type: "json" };
import { LescheClient } from "./http.ts";
import type { Envelope } from "./types.ts";

const ROUTES: ReadonlyArray<readonly string[]> = shapes.routes;

function dialledRouteExists(verb: string, path: string): boolean {
  return ROUTES.some((pair) => {
    const verb0 = pair[0];
    const pattern0 = pair[1];
    if (verb0 === undefined || pattern0 === undefined) return false;
    if (verb0 !== verb && verb0 !== "ANY") return false;
    const pattern = pattern0
      .replaceAll("{*path}", ".+")
      .replaceAll(/\{[^}/]+\}/g, "[^/]+");
    return new RegExp(`^${pattern}$`).test(path);
  });
}

Deno.test(
  "every client-dialled URL exists in the shared route fixture",
  async () => {
    const real = globalThis.fetch;
    const seen: Array<[string, string]> = [];
    globalThis.fetch = ((url: string | URL, init?: RequestInit) => {
      const path = String(url).replace("http://x", "").replace(/\?.*$/, "");
      seen.push([init?.method ?? "GET", path]);
      return Promise.resolve(Response.json({}));
    }) as typeof fetch;
    try {
      const client = new LescheClient("http://x");
      await client.postRoomEnvelope("r1", {} as Envelope);
      await client.fetchRoomMessages("r1", { afterSeq: 3, limit: 5 });
      await client.createRoom({ name: "n" });
      await client.listRooms();
      await client.listPublicRooms();
      await client.joinRoom("r1");
      await client.setRoomReadCursor("r1", 7);
      await client.fetchRoomRoster("r1");
      await client.listMyRoomInvites();
      await client.createRoomInvite("r1", "@u");
      await client.acceptRoomInvite("r1", "i1");
      await client.removeRoomMember("r1", "m1");
      await client.addRoomTagma("r1", "t-a");
      await client.listMyTagmaRooms("t-a");
      const events = client.meEvents();
      await events.next();
      await events.return(undefined);
      assertEquals(seen.length, 15);
      for (const [verb, path] of seen) {
        if (!dialledRouteExists(verb, path)) {
          throw new Error(`dialled URL not in shared fixture: ${verb} ${path}`);
        }
      }
    } finally {
      globalThis.fetch = real;
    }
  },
);
