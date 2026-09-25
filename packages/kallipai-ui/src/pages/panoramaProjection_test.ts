import { assertEquals } from "@std/assert";
import { panoramaSessionRows } from "./panoramaProjection.ts";

Deno.test(
  "unread rows pin above read rows, source order kept in each group",
  () => {
    const rows = [
      { href: "/a", label: "a", kind: "tagma" as const },
      { href: "/b", label: "b", kind: "room" as const, badge: 2 },
      { href: "/c", label: "c", kind: "direct" as const },
      { href: "/d", label: "d", kind: "room" as const, badge: 1 },
    ];
    assertEquals(
      panoramaSessionRows(rows, 10).map((r) => r.href),
      ["/b", "/d", "/a", "/c"],
    );
  },
);

Deno.test("cap truncates after the unread block", () => {
  const rows = [
    { href: "/u", label: "u", kind: "tagma" as const, badge: 3 },
    { href: "/r1", label: "r1", kind: "room" as const },
    { href: "/r2", label: "r2", kind: "direct" as const },
  ];
  assertEquals(
    panoramaSessionRows(rows, 2).map((r) => r.href),
    ["/u", "/r1"],
  );
});

Deno.test("all-read list keeps source order under the cap", () => {
  const rows = [
    { href: "/x", label: "x", kind: "room" as const },
    { href: "/y", label: "y", kind: "tagma" as const },
  ];
  assertEquals(
    panoramaSessionRows(rows, 1).map((r) => r.href),
    ["/x"],
  );
});

Deno.test("empty input stays empty at any cap", () => {
  assertEquals(panoramaSessionRows([], 6), []);
});
