// Chain pins for the wired trail table: a drill page's chain must extend
// its list page's chain verbatim and append, and every entry opens with
// the Home segment (target /) -- the bar's universal first crumb
// (operator-set IA rule). Same shim discipline as
// breadcrumbs_mobileback_test: the module under test pulls rune-bearing
// stores and compiled paraglide messages, so the passthrough shims
// precede the dynamic import.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  function $derived<T>(expr: T): T;
}
(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
(globalThis as Record<string, unknown>)["$derived"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { matchTrail, trailTable } = await import("./breadcrumbs.ts");
const {
  nav_chats,
  nav_files,
  nav_home,
  nav_rooms,
  room_label_fallback,
  settings_heading,
} = await import("../../paraglide/messages.js");

Deno.test("the files page keeps its home-first chain", () => {
  assertEquals(matchTrail("/files"), [
    { label: nav_home(), href: "/" },
    { label: nav_files(), current: true },
  ]);
});
Deno.test("the rooms list keeps its home-first chain", () => {
  assertEquals(matchTrail("/rooms"), [
    { label: nav_home(), href: "/" },
    { label: nav_rooms(), current: true },
  ]);
});

Deno.test("the room drill extends the list chain with the room", () => {
  assertEquals(matchTrail("/rooms/r-1"), [
    { label: nav_home(), href: "/" },
    { label: nav_rooms(), href: "/rooms" },
    { label: room_label_fallback({ id: "r-1" }), current: true },
  ]);
});

Deno.test("the room settings drill keeps the full chain", () => {
  assertEquals(matchTrail("/rooms/r-1/settings"), [
    { label: nav_home(), href: "/" },
    { label: nav_rooms(), href: "/rooms" },
    {
      label: room_label_fallback({ id: "r-1" }),
      href: "/rooms/r-1",
    },
    { label: settings_heading(), current: true },
  ]);
});

Deno.test("drill chains extend their list chain (shape continuity)", () => {
  const list = matchTrail("/rooms");
  const drill = matchTrail("/rooms/r-1");
  if (!list || !drill) throw new Error("the rooms chains must resolve");
  assertEquals(
    drill.slice(0, list.length).map((s) => s.label),
    list.map((s) => s.label),
  );
  // the list page's tail becomes a link in the drill chain
  assertEquals(drill[list.length - 1]?.href, "/rooms");
});

Deno.test("every entry opens with the home segment", () => {
  for (const { pattern } of trailTable) {
    // Synthesized concrete paths: ':section' pins to 'overview' (a valid
    // enumerated member); other params get a placeholder 'x-1'.
    const path = pattern
      .split("/")
      .map((seg) =>
        seg === ":section" ? "overview" : seg.startsWith(":") ? "x-1" : seg,
      )
      .join("/");
    const trail = matchTrail(path);
    if (!trail) throw new Error(`${path} must resolve`);
    assertEquals(
      trail[0],
      { label: nav_home(), href: "/" },
      `${path} must open with Home`,
    );
  }
});

Deno.test("conversations chain through their chats-hub domain", () => {
  const paths = ["/chat/c-1", "/tagma/t-1/chat", "/tagma/t-1/direct/p-1"];
  for (const path of paths) {
    const trail = matchTrail(path);
    if (!trail) throw new Error(`${path} must resolve`);
    assertEquals(trail[1], { label: nav_chats(), href: "/chats" });
  }
});
