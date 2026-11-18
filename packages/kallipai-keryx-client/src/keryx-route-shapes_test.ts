// Two-end reconciliation for the keryx route table: the shared fixture the
// rust shape test asserts against (crates/platform/kallipai-keryx/
// src/route-shapes.json) must also load and hold its shape on the TS side,
// so the import path and the fixture format are pinned, and every URL the
// package's clients dial must exist in it -- the bilateral client, the
// projection reads, and the manage proxy. A one-sided URL change fails a
// test on both ends. Query strings are outside the pin: the stub strips
// them before matching, so a query drift is not caught here.
// The four rows no TS dial touches: the key-exchange response and the
// tunnel pair are Rust-client faces, and the tagma upstream row is the
// tagma-side dial.

import { withFetch } from "@kallipai/kallipai-common/testing";
import { assertEquals } from "@std/assert";
import shapes from "../../../crates/platform/kallipai-keryx/src/route-shapes.json" with { type: "json" };
import { KeryxClient } from "./client.ts";
import { ManageRestClient } from "./manageRest.ts";
import { ProjectionClient } from "./projection.ts";
import type { Envelope, KeyExchangeInit } from "./types.ts";

const BASE = "http://x";

const ROUTES: ReadonlyArray<readonly string[]> = shapes.routes;

Deno.test(
  "keryx shared route fixture loads with well-formed sorted rows",
  () => {
    if (ROUTES.length === 0) {
      throw new Error("keryx fixture must carry at least one route row");
    }
    const paths: Array<string> = [];
    for (const pair of ROUTES) {
      const verb = pair[0];
      const path = pair[1];
      if (verb === undefined || path === undefined) {
        throw new Error("keryx fixture row is not a [verb, path] pair");
      }
      if (!/^(GET|POST|PUT|DELETE|PATCH|ANY)$/.test(verb)) {
        throw new Error(`keryx fixture carries an unknown verb: ${verb}`);
      }
      if (!path.startsWith("/") || path.includes("?")) {
        throw new Error(
          `keryx fixture path is not a bare server shape: ${path}`,
        );
      }
      paths.push(path);
    }
    const sorted = [...paths].sort();
    for (let i = 0; i < paths.length; i++) {
      if (paths[i] !== sorted[i]) {
        throw new Error(
          "keryx fixture rows must stay path-sorted at index " + i,
        );
      }
    }
  },
);

function dialledRouteExists(verb: string, path: string): boolean {
  return ROUTES.some((pair) => {
    const verb0 = pair[0];
    const pattern0 = pair[1];
    if (verb0 === undefined || pattern0 === undefined) return false;
    if (verb0 !== verb && verb0 !== "ANY") return false;
    const pattern = pattern0
      .replaceAll("{*path}", ".+")
      .replaceAll(/\{[^}/]+}/g, "[^/]+");
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
    dial: (client: KeryxClient) => Promise<unknown>,
  ]
> = [
  [
    "createConversation",
    "POST",
    "/conversations",
    (c) => c.createConversation("t-a"),
  ],
  [
    "keyExchangeInit",
    "POST",
    "/conversations/c1/key-exchange/init",
    (c) => c.keyExchangeInit("c1", {} as KeyExchangeInit),
  ],
  [
    "postEnvelope",
    "POST",
    "/conversations/c1/envelopes",
    (c) => c.postEnvelope("c1", {} as Envelope),
  ],
  [
    "keryxEvents",
    "GET",
    "/me/events",
    async (c) => {
      const events = c.keryxEvents();
      await events.next();
      await events.return(undefined);
    },
  ],
];

/** The projection read client is not the bilateral surface, so its dials are
 * pinned separately with the same one-dial/one-route rule. */
const PROJECTION_DIALS: ReadonlyArray<
  readonly [
    name: string,
    verb: string,
    path: string,
    dial: (client: ProjectionClient) => Promise<unknown>,
  ]
> = [
  ["agents", "GET", "/tagmata/t-a/agents", (c) => c.agents("t-a")],
  ["budget", "GET", "/tagmata/t-a/budget", (c) => c.budget("t-a")],
  [
    "workSchedule",
    "GET",
    "/tagmata/t-a/work-schedule",
    (c) => c.workSchedule("t-a"),
  ],
  ["status", "GET", "/tagmata/t-a/status", (c) => c.status("t-a")],
  [
    "state",
    "GET",
    "/tagmata/t-a/state",
    async (c) => {
      const states = c.state("t-a");
      await states.next();
      await states.return(undefined);
    },
  ],
];

Deno.test(
  "every client-dialled URL exists in the shared route fixture",
  async () => {
    const seen: Array<[string, string]> = [];
    await withFetch(
      ((url: string | URL, init?: RequestInit) => {
        const path = String(url).replace(BASE, "").replace(/\?.*$/, "");
        seen.push([init?.method ?? "GET", path]);
        return Promise.resolve(Response.json({}));
      }) as typeof fetch,
      async () => {
        const client = new KeryxClient(BASE);
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
        // The dial table and the surface must name the same methods: a verb
        // added without a dial fails here instead of slipping past the fixture.
        assertEquals(
          DIALS.map(([name]) => name).sort(),
          Object.getOwnPropertyNames(KeryxClient.prototype)
            .filter((name) => name !== "constructor")
            .sort(),
        );

        const projection = new ProjectionClient(BASE);
        for (const [name, verb, path, dial] of PROJECTION_DIALS) {
          const before = seen.length;
          await dial(projection);
          assertEquals(seen.length, before + 1, `${name} must dial once`);
          assertEquals(
            seen[before],
            [verb, path],
            `${name} must reach its route`,
          );
        }
        assertEquals(
          PROJECTION_DIALS.map(([name]) => name).sort(),
          // `get` is the private fetch primitive those five verbs share, not a
          // dial of its own, so it is not in the table.
          Object.getOwnPropertyNames(ProjectionClient.prototype)
            .filter((name) => name !== "constructor" && name !== "get")
            .sort(),
        );

        const manage = new ManageRestClient(BASE);
        await manage.manage("t-a", "GET", "/agents");

        for (const [verb, path] of seen) {
          if (!dialledRouteExists(verb, path)) {
            throw new Error(
              `dialled URL not in shared fixture: ${verb} ${path}`,
            );
          }
        }
      },
    );
  },
);
