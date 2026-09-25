import { assertEquals } from "@std/assert";
import {
  backFromTrail,
  entry,
  matchTrail,
  type TrailEntry,
} from "./trailMatch.ts";

// Behavior tests for the matcher engine, on a synthetic table (the real
// table's resolvers pull session stores in, which a deno test cannot load).
// Covered here: segment-count strictness, first-match precedence, capture
// decoding (including malformed escapes), the off-table null, and the
// backFromTrail walk to the deepest linked segment.

const table: TrailEntry[] = [
  entry("/tagmata", () => [{ label: "tagmata", current: true }]),
  entry("/tagma/:id/chat", ({ id }) => [
    { label: "tagma", href: `/tagma/${id}/details` },
    { label: "chat", current: true },
  ]),
  entry("/tagma/:id/details/:section", ({ id, section }) => [
    { label: "tagma", href: `/tagma/${id}/details` },
    { label: section, current: true },
  ]),
  entry("/rooms/:id", ({ id }) => [{ label: id, current: true }]),
  entry("/rooms/:id/settings", ({ id }) => [
    { label: id, href: `/rooms/${id}` },
    { label: "settings", current: true },
  ]),
  entry("/user/:handle", ({ handle }) => [{ label: handle, current: true }]),
];

Deno.test(
  "segments must match exactly in count: /rooms/:id does not swallow its settings subpage",
  () => {
    assertEquals(matchTrail(table, "/rooms/abc"), [
      { label: "abc", current: true },
    ]);
    assertEquals(matchTrail(table, "/rooms/abc/settings"), [
      { label: "abc", href: "/rooms/abc" },
      { label: "settings", current: true },
    ]);
    assertEquals(matchTrail(table, "/rooms/abc/settings/extra"), null);
  },
);

Deno.test(
  "first match wins among same-length patterns (literal segments are disjoint)",
  () => {
    // /tagmata (1 literal segment) vs nothing else 1-long: exact hit.
    assertEquals(matchTrail(table, "/tagmata"), [
      { label: "tagmata", current: true },
    ]);
    // A 2-segment path no pattern claims -> null, not a partial match.
    assertEquals(matchTrail(table, "/tagma"), null);
  },
);

Deno.test(
  "captures decode once, aligned with framework params; malformed escapes fall back to raw",
  () => {
    assertEquals(matchTrail(table, "/user/a%20b"), [
      { label: "a b", current: true },
    ]);
    assertEquals(matchTrail(table, "/user/100%"), [
      { label: "100%", current: true },
    ]);
    // A %2F in a capture stays within the segment: it must not split the path.
    const segs = matchTrail(table, "/rooms/a%2Fb");
    assertEquals(segs, [{ label: "a/b", current: true }]);
  },
);

Deno.test(
  "off-table routes yield null (the shell renders no bar there)",
  () => {
    assertEquals(matchTrail(table, "/local/chat"), null);
    assertEquals(matchTrail(table, "/"), null);
    assertEquals(matchTrail(table, ""), null);
  },
);

Deno.test("backFromTrail walks to the deepest linked segment", () => {
  assertEquals(
    backFromTrail([
      { label: "rooms", href: "/rooms" },
      { label: "r1", href: "/rooms/r1" },
      { label: "settings", current: true },
    ]),
    { href: "/rooms/r1", label: "r1" },
  );
  // A chain with a middle link stops at the last link, skipping the pure tail.
  assertEquals(
    backFromTrail([
      { label: "tagma", href: "/tagma/t1/details" },
      { label: "chat", current: true },
    ]),
    { href: "/tagma/t1/details", label: "tagma" },
  );
});

Deno.test(
  "backFromTrail yields null when no segment links (a destination, not a drill)",
  () => {
    assertEquals(backFromTrail([{ label: "tagmata", current: true }]), null);
    assertEquals(backFromTrail([]), null);
  },
);
