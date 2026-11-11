// The matcher replaces SvelteKit's own route resolution for the host shells,
// so these cases pin the kit behaviours the route trees relied on: root, one
// trailing slash, decoded params (escaped percent, escaped slash, non-ASCII),
// case sensitivity, the admin subtree's longer path, and no match.
import { assertEquals } from "@std/assert";
import { matchPath } from "./routeMatch.ts";

Deno.test("root pattern matches the root pathname only", () => {
  assertEquals(matchPath("", "/"), {});
  assertEquals(matchPath("", "/chats"), null);
  assertEquals(matchPath("chats", "/"), null);
});

Deno.test("one trailing slash is tolerated", () => {
  assertEquals(matchPath("chats", "/chats"), {});
  assertEquals(matchPath("chats", "/chats/"), {});
  // Two trailing slashes leave an empty segment, which kit does not match.
  assertEquals(matchPath("chats", "/chats//"), null);
  assertEquals(matchPath("chats", "/chats/extra"), null);
});

Deno.test("params decode once, in the kit order", () => {
  assertEquals(matchPath("chat/[id]", "/chat/abc"), { id: "abc" });
  // An escaped percent stays encoded through the pathname decode and is
  // decoded once for the param, so the value is a single "%".
  assertEquals(matchPath("user/[handle]", "/user/50%25"), { handle: "50%" });
  // An escaped slash is a reserved character, so the pathname decode leaves
  // it alone and the param decode turns it into a real slash.
  assertEquals(matchPath("user/[handle]", "/user/a%2Fb"), { handle: "a/b" });
  assertEquals(matchPath("user/[handle]", "/user/%E5%BC%A0%E4%B8%89"), {
    handle: "张三",
  });
  // Several params in one pattern.
  assertEquals(
    matchPath("tagma/[id]/direct/[peer]", "/tagma/t1/direct/peer%20two"),
    { id: "t1", peer: "peer two" },
  );
});

Deno.test("matching is case sensitive and segment exact", () => {
  assertEquals(matchPath("chats", "/Chats"), null);
  assertEquals(matchPath("Chats", "/chats"), null);
  assertEquals(matchPath("rooms/[id]", "/rooms"), null);
  assertEquals(matchPath("rooms/[id]", "/rooms/a/b"), null);
});

Deno.test("admin subtree matches its own depth", () => {
  assertEquals(matchPath("admin/gateway", "/admin/gateway"), {});
  assertEquals(
    matchPath("admin/gateway/collections/[name]", "/admin/gateway"),
    null,
  );
  assertEquals(
    matchPath(
      "admin/gateway/collections/[name]",
      "/admin/gateway/collections/tools",
    ),
    { name: "tools" },
  );
});

Deno.test("a path no pattern covers yields no match", () => {
  assertEquals(matchPath("chats", "/nope"), null);
  assertEquals(matchPath("tagma/[id]", "/tagma/t1/nope"), null);
});
