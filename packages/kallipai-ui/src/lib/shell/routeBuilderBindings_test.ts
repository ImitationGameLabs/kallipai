// The path builders in routes.ts and the route table are one piece of URL
// knowledge written twice: a builder hands a URL out, a row must match it
// back. The table cannot be imported here (it pulls in .svelte components),
// so each built path is matched against ROUTE_PATTERNS, which the table pins
// to itself with a type-level equality check.
import { assertEquals } from "@std/assert";
import { matchPath, type Params } from "./routeMatch.ts";
import { ROUTE_PATTERNS } from "./routePatterns.ts";
import {
  adminGatewayCollectionPath,
  adminGatewayPath,
  agentHistoryPath,
  filesPath,
  gatewayPath,
  roomPath,
  roomThreadPath,
  tagmaAgentPath,
  tagmaChatPath,
  tagmaDetailsPath,
  tagmaDetailsSectionPath,
} from "./routes.ts";

/** The single pattern that matches this path, with the params it captured. */
function matchOne(path: string): { pattern: string; params: Params } {
  const hits = ROUTE_PATTERNS.flatMap((pattern) => {
    const params = matchPath(pattern, path);
    return params === null ? [] : [{ pattern, params }];
  });
  assertEquals(hits.length, 1, `${path} matched ${hits.length} rows`);
  return hits[0]!;
}

Deno.test("every path builder lands on exactly the row it names", () => {
  const cases: [string, string, Params][] = [
    [tagmaChatPath("abc"), "tagma/[id]/chat", { id: "abc" }],
    [tagmaDetailsPath("abc"), "tagma/[id]/details", { id: "abc" }],
    [
      tagmaDetailsSectionPath("abc", "budget"),
      "tagma/[id]/details/budget",
      { id: "abc" },
    ],
    [
      tagmaAgentPath("abc", "ag1"),
      "tagma/[id]/details/agents/[agentId]",
      { id: "abc", agentId: "ag1" },
    ],
    [
      agentHistoryPath(tagmaDetailsPath("abc"), "ag1"),
      "tagma/[id]/details/agents/[agentId]/history",
      { id: "abc", agentId: "ag1" },
    ],
    [filesPath(), "files", {}],
    [gatewayPath(), "gateway", {}],
    [adminGatewayPath(), "admin/gateway", {}],
    [roomPath("abc"), "rooms/[id]", { id: "abc" }],
    [
      roomThreadPath("abc", 7),
      "rooms/[id]/threads/[root]",
      { id: "abc", root: "7" },
    ],
    [
      adminGatewayCollectionPath("alpha"),
      "admin/gateway/collections/[name]",
      { name: "alpha" },
    ],
  ];
  for (const [path, pattern, params] of cases) {
    const hit = matchOne(path);
    assertEquals(hit.pattern, pattern, path);
    assertEquals(hit.params, params, path);
  }
});

Deno.test("a room path carries the row it lands on", () => {
  assertEquals(roomPath("abc"), "/rooms/abc");
  assertEquals(roomPath("abc", { at: 7 }), "/rooms/abc?at=7");
});

Deno.test("a discussion path carries its root and the rows it names", () => {
  assertEquals(roomThreadPath("abc", 7), "/rooms/abc/threads/7");
  assertEquals(
    roomThreadPath("abc", 7, { at: 12 }),
    "/rooms/abc/threads/7?at=12",
  );
  assertEquals(
    roomThreadPath("abc", 7, { quote: 12 }),
    "/rooms/abc/threads/7?quote=12",
  );
  assertEquals(
    roomThreadPath("abc", 7, { at: 3, quote: 12 }),
    "/rooms/abc/threads/7?at=3&quote=12",
  );
});
