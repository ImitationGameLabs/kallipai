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
import { LescheClient } from "./client.ts";
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

/** One dial per method the class exposes, each with the route it must reach.
 * The assertions below compare this table's names with the prototype's and
 * each dial with its declared route, so a method added without a dial, or a
 * verb re-pointed at another route that already exists, fails here. */
const DIALS: ReadonlyArray<
  readonly [
    name: string,
    verb: string,
    path: string,
    dial: (client: LescheClient) => Promise<unknown>,
  ]
> = [
  [
    "postRoomEnvelope",
    "POST",
    "/rooms/r1/envelopes",
    (c) => c.postRoomEnvelope("r1", {} as Envelope),
  ],
  [
    "fetchRoomMessages",
    "GET",
    "/rooms/r1/messages",
    (c) => c.fetchRoomMessages("r1", { afterSeq: 3, limit: 5 }),
  ],
  ["createRoom", "POST", "/rooms", (c) => c.createRoom({ name: "n" })],
  ["listRooms", "GET", "/rooms", (c) => c.listRooms()],
  ["listPublicRooms", "GET", "/rooms/public", (c) => c.listPublicRooms()],
  ["joinRoom", "POST", "/rooms/r1/join", (c) => c.joinRoom("r1")],
  [
    "setRoomReadCursor",
    "PUT",
    "/rooms/r1/read-cursor",
    (c) => c.setRoomReadCursor("r1", 7),
  ],
  [
    "fetchRoomUnread",
    "GET",
    "/rooms/r1/unread",
    (c) => c.fetchRoomUnread("r1"),
  ],
  [
    "fetchThreadSubscription",
    "GET",
    "/rooms/r1/threads/3/subscription",
    (c) => c.fetchThreadSubscription("r1", 3),
  ],
  [
    "setThreadSubscription",
    "PUT",
    "/rooms/r1/threads/3/subscription",
    (c) => c.setThreadSubscription("r1", 3, "following"),
  ],
  [
    "clearThreadSubscription",
    "DELETE",
    "/rooms/r1/threads/3/subscription",
    (c) => c.clearThreadSubscription("r1", 3),
  ],
  ["fetchRoomRoster", "GET", "/rooms/r1", (c) => c.fetchRoomRoster("r1")],
  ["listMyRoomInvites", "GET", "/rooms/invites", (c) => c.listMyRoomInvites()],
  [
    "createRoomInvite",
    "POST",
    "/rooms/r1/invites",
    (c) => c.createRoomInvite("r1", "@u"),
  ],
  [
    "acceptRoomInvite",
    "POST",
    "/rooms/r1/invites/i1/accept",
    (c) => c.acceptRoomInvite("r1", "i1"),
  ],
  [
    "removeRoomMember",
    "DELETE",
    "/rooms/r1/members/m1",
    (c) => c.removeRoomMember("r1", "m1"),
  ],
  [
    "addRoomTagma",
    "POST",
    "/rooms/r1/tagmata",
    (c) => c.addRoomTagma("r1", "t-a"),
  ],
  [
    "listMyTagmaRooms",
    "GET",
    "/me/tagmata/t-a/rooms",
    (c) => c.listMyTagmaRooms("t-a"),
  ],
  [
    "meEvents",
    "GET",
    "/me/events",
    async (c) => {
      const events = c.meEvents();
      await events.next();
      await events.return(undefined);
    },
  ],
];

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
      for (const [name, verb, path, dial] of DIALS) {
        const before = seen.length;
        await dial(client);
        assertEquals(seen.length, before + 1, `${name} must dial once`);
        assertEquals(
          seen[before],
          [verb, path],
          `${name} must reach its route`,
        );
      }
      assertEquals(seen.length, DIALS.length);
      // The dial table and the surface must name the same methods: a verb
      // added without a dial fails here instead of slipping past the fixture.
      assertEquals(
        DIALS.map(([name]) => name).sort(),
        Object.getOwnPropertyNames(LescheClient.prototype)
          .filter((name) => name !== "constructor")
          .sort(),
      );
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
