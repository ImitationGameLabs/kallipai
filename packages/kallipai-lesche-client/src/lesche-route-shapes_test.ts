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
import { runClientEffect, runClientStream } from "@kallipai/kallipai-common";
import {
  acceptRoomInvite,
  addRoomTagma,
  clearThreadSubscription,
  createRoom,
  createRoomInvite,
  fetchRoomMessages,
  fetchRoomRoster,
  fetchRoomUnread,
  fetchThreadSubscription,
  joinRoom,
  listMyTagmaRooms,
  listMyRoomInvites,
  listPublicRooms,
  listRooms,
  postRoomEnvelope,
  removeRoomMember,
  roomEvents,
  setRoomReadCursor,
  setThreadSubscription,
} from "./client.ts";
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

/** One dial per verb the module exports, each with the route it must reach.
 * The assertions below compare this table's names with the module's function
 * exports and each dial with its declared route, so a verb added without a
 * dial, a dial row naming nothing, or a verb re-pointed at another route that
 * already exists, fails here. */
const DIALS: ReadonlyArray<
  readonly [
    name: string,
    verb: string,
    path: string,
    dial: (base: string) => Promise<unknown>,
  ]
> = [
  [
    "postRoomEnvelope",
    "POST",
    "/rooms/r1/envelopes",
    (b) => runClientEffect(postRoomEnvelope(b, "r1", {} as Envelope)),
  ],
  [
    "fetchRoomMessages",
    "GET",
    "/rooms/r1/messages",
    (b) =>
      runClientEffect(fetchRoomMessages(b, "r1", { afterSeq: 3, limit: 5 })),
  ],
  [
    "createRoom",
    "POST",
    "/rooms",
    (b) => runClientEffect(createRoom(b, { name: "n" })),
  ],
  ["listRooms", "GET", "/rooms", (b) => runClientEffect(listRooms(b))],
  [
    "listPublicRooms",
    "GET",
    "/rooms/public",
    (b) => runClientEffect(listPublicRooms(b)),
  ],
  [
    "joinRoom",
    "POST",
    "/rooms/r1/join",
    (b) => runClientEffect(joinRoom(b, "r1")),
  ],
  [
    "setRoomReadCursor",
    "PUT",
    "/rooms/r1/read-cursor",
    (b) => runClientEffect(setRoomReadCursor(b, "r1", 7)),
  ],
  [
    "fetchRoomUnread",
    "GET",
    "/rooms/r1/unread",
    (b) => runClientEffect(fetchRoomUnread(b, "r1")),
  ],
  [
    "fetchThreadSubscription",
    "GET",
    "/rooms/r1/threads/3/subscription",
    (b) => runClientEffect(fetchThreadSubscription(b, "r1", 3)),
  ],
  [
    "setThreadSubscription",
    "PUT",
    "/rooms/r1/threads/3/subscription",
    (b) => runClientEffect(setThreadSubscription(b, "r1", 3, "following")),
  ],
  [
    "clearThreadSubscription",
    "DELETE",
    "/rooms/r1/threads/3/subscription",
    (b) => runClientEffect(clearThreadSubscription(b, "r1", 3)),
  ],
  [
    "fetchRoomRoster",
    "GET",
    "/rooms/r1",
    (b) => runClientEffect(fetchRoomRoster(b, "r1")),
  ],
  [
    "listMyRoomInvites",
    "GET",
    "/rooms/invites",
    (b) => runClientEffect(listMyRoomInvites(b)),
  ],
  [
    "createRoomInvite",
    "POST",
    "/rooms/r1/invites",
    (b) => runClientEffect(createRoomInvite(b, "r1", "@u")),
  ],
  [
    "acceptRoomInvite",
    "POST",
    "/rooms/r1/invites/i1/accept",
    (b) => runClientEffect(acceptRoomInvite(b, "r1", "i1")),
  ],
  [
    "removeRoomMember",
    "DELETE",
    "/rooms/r1/members/m1",
    (b) => runClientEffect(removeRoomMember(b, "r1", "m1")),
  ],
  [
    "addRoomTagma",
    "POST",
    "/rooms/r1/tagmata",
    (b) => runClientEffect(addRoomTagma(b, "r1", "t-a")),
  ],
  [
    "listMyTagmaRooms",
    "GET",
    "/me/tagmata/t-a/rooms",
    (b) => runClientEffect(listMyTagmaRooms(b, "t-a")),
  ],
  [
    "roomEvents",
    "GET",
    "/me/events",
    async (b) => {
      const events = runClientStream(roomEvents(b));
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
      for (const [name, verb, path, dial] of DIALS) {
        const before = seen.length;
        await dial("http://x");
        assertEquals(seen.length, before + 1, `${name} must dial once`);
        assertEquals(
          seen[before],
          [verb, path],
          `${name} must reach its route`,
        );
      }
      assertEquals(seen.length, DIALS.length);
      // The dial table and the module's verbs must name the same set: a verb
      // added without a dial, or a dial row naming nothing, fails here.
      assertEquals(
        DIALS.map(([name]) => name).sort(),
        Object.entries(await import("./client.ts"))
          .filter(([, value]) => typeof value === "function")
          .map(([name]) => name)
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
