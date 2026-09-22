// Two-end reconciliation: every URL the TS clients
// dial must exist in the shared route fixture the rust shape test also
// asserts against. A one-sided URL change now fails a test on both sides --
// the original double-prefix bugs came from each end proving itself alone.
// Dialled URLs arrive in the edge shape (/v1/lesche/...); they are
// translated to the server shape (bare paths) before matching the fixture.
// (The /state SSE dial is pinned verbatim in projection.test.ts; this file
// covers the parameterized templates.)

import { ManageRestClient } from "./manageRest.ts";
import { ProjectionClient } from "./projection.ts";
import shapes from "../../../crates/platform/kallip-lesche/src/route-shapes.json" with { type: "json" };

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
      const path = String(url)
        .replace("https://lesche.example", "")
        .replace(/^\/v1\/lesche/, ""); // edge shape -> bare server shape
      seen.push([init?.method ?? "GET", path]);
      return Promise.resolve(Response.json({}));
    }) as typeof fetch;
    try {
      const manage = new ManageRestClient("https://lesche.example/v1/lesche");
      await manage.manage("t-a", "GET", "/agents");
      await manage.manage("t-a", "POST", "/budget", {});
      await manage.manage("t-a", "PUT", "/profiles", {});
      await manage.manage("t-a", "DELETE", "/profiles/9");
      const projection = new ProjectionClient(
        "https://lesche.example/v1/lesche",
      );
      await projection.agents("t-a");
      await projection.budget("t-a");
      await projection.workSchedule("t-a");
      await projection.status("t-a");
      for (const [verb, path] of seen) {
        if (verb === undefined || path === undefined) continue;
        if (!dialledRouteExists(verb, path)) {
          throw new Error(`dialled URL not in shared fixture: ${verb} ${path}`);
        }
      }
    } finally {
      globalThis.fetch = real;
    }
  },
);
