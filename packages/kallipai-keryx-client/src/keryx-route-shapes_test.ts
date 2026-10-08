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

import { assertEquals } from "@std/assert";
import shapes from "../../../crates/platform/kallipai-keryx/src/route-shapes.json" with { type: "json" };
import { KeryxClient } from "./http.ts";
import { ManageRestClient } from "./manageRest.ts";
import { ProjectionClient } from "./projection.ts";
import type { Envelope, KeyExchangeInit } from "./types.ts";

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
      const client = new KeryxClient("http://x");
      await client.createConversation("t-a");
      await client.keyExchangeInit("c1", {} as KeyExchangeInit);
      await client.postEnvelope("c1", {} as Envelope);
      const events = client.keryxEvents();
      await events.next();
      await events.return(undefined);
      const projection = new ProjectionClient("http://x");
      await projection.agents("t-a");
      await projection.budget("t-a");
      await projection.workSchedule("t-a");
      await projection.status("t-a");
      const state = projection.state("t-a");
      await state.next();
      await state.return(undefined);
      const manage = new ManageRestClient("http://x");
      await manage.manage("t-a", "GET", "/agents");
      assertEquals(seen.length, 10);
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
