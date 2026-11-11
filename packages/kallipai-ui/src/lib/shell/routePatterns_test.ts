// Two properties of the URL list that no type can carry: no pattern
// appears twice, so no row shadows another, and the list still holds 32
// entries. The count is deliberate: the table and this list are deleted
// together, and the type equality still holds when both shrink, so the
// number is the one gate left against a route disappearing silently.
import { assertEquals } from "@std/assert";
import { ROUTE_PATTERNS } from "./routePatterns.ts";

Deno.test("the URL list holds 32 patterns, once each", () => {
  assertEquals(ROUTE_PATTERNS.length, 32);
  assertEquals(new Set(ROUTE_PATTERNS).size, ROUTE_PATTERNS.length);
});
