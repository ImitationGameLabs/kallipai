// Tests for the mobile back-row route policy (lib/shell/breadcrumbs.ts
// mobileBack): conversations chain through their chats-hub domain now
// that the trail table itself is home-first, so their back target falls
// out of the trail with no special case; the tagma details sections
// stay on the bottom bar (null -- the manage cell lights via RootLayout
// isActive); the agent detail and the tagma hub keep their trail
// drill; bar-cell destinations (/account, /tagmata, /files) never grow
//
// The module under test transitively imports rune-bearing stores and
// the compiled paraglide messages, so the $state / $derived passthrough
// shims are declared before the dynamic import (deno test runs the
// modules uncompiled) -- the directSessions_test pattern.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  function $derived<T>(expr: T): T;
}
(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
(globalThis as Record<string, unknown>)["$derived"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { mobileBack } = await import("./breadcrumbs.ts");

Deno.test("the tagma chat drills back to the chats hub", () => {
  assertEquals(mobileBack("/tagma/t-1/chat")?.href, "/chats");
});

Deno.test("the relay chat keeps its chats-hub back target", () => {
  assertEquals(mobileBack("/chat/c-1")?.href, "/chats");
});
Deno.test("the direct session keeps its chats-hub back target", () => {
  assertEquals(mobileBack("/tagma/t-1/direct/p-1")?.href, "/chats");
});

Deno.test("the tagma details sections keep the bottom bar", () => {
  assertEquals(mobileBack("/tagma/t-1/details/overview"), null);
  assertEquals(mobileBack("/tagma/t-1/details/budget"), null);
  assertEquals(mobileBack("/tagma/t-1/details/agents"), null);
});
Deno.test("the profiles section keeps the bottom bar", () => {
  assertEquals(mobileBack("/tagma/t-1/details/profiles"), null);
});

Deno.test("the schedules section keeps the bottom bar", () => {
  assertEquals(mobileBack("/tagma/t-1/details/schedules"), null);
});

Deno.test("the agent detail stays a drill to its agents section", () => {
  assertEquals(
    mobileBack("/tagma/t-1/details/agents/a-9")?.href,
    "/tagma/t-1/details/agents",
  );
});

Deno.test("the tagma hub keeps its registry drill", () => {
  assertEquals(mobileBack("/tagma/t-1")?.href, "/tagmata");
});

Deno.test("bar-cell destinations never grow a back row", () => {
  assertEquals(mobileBack("/account"), null);
  assertEquals(mobileBack("/tagmata"), null);
  assertEquals(mobileBack("/files"), null);
});

Deno.test("home-first chains back domain-less pages to Home", () => {
  assertEquals(mobileBack("/settings")?.href, "/");
  assertEquals(mobileBack("/user/someone")?.href, "/");
  assertEquals(mobileBack("/rooms")?.href, "/");
});

Deno.test("room drills keep their list-page back target", () => {
  assertEquals(mobileBack("/rooms/r-1")?.href, "/rooms");
  assertEquals(mobileBack("/rooms/r-1/settings")?.href, "/rooms/r-1");
});
