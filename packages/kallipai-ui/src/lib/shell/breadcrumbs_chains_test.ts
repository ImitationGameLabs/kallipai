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
  manage_gateway_menu,
  nav_admin,
  nav_breadcrumb_agents,
  nav_breadcrumb_tagma,
  user_gateway_menu,
  nav_chats,
  manage_history_heading,
  nav_files,
  nav_home,
  nav_rooms,
  room_label_fallback,
  room_thread_view,
  settings_heading,
  account_menu,
  account_tokens,
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
Deno.test("the token page extends the account chain", () => {
  assertEquals(matchTrail("/account/tokens"), [
    { label: nav_home(), href: "/" },
    { label: account_menu(), href: "/account" },
    { label: account_tokens(), current: true },
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

Deno.test("the admin gateway console chains through the admin hub", () => {
  assertEquals(matchTrail("/admin/gateway"), [
    { label: nav_home(), href: "/" },
    { label: nav_admin() },
    { label: manage_gateway_menu(), current: true },
  ]);
});

Deno.test("the collection detail drill keeps the full chain", () => {
  assertEquals(matchTrail("/admin/gateway/collections/gpt"), [
    { label: nav_home(), href: "/" },
    { label: nav_admin() },
    { label: manage_gateway_menu(), href: "/admin/gateway" },
    { label: "gpt", current: true },
  ]);
});

Deno.test("the user gateway page keeps its home-first chain", () => {
  assertEquals(matchTrail("/gateway"), [
    { label: nav_home(), href: "/" },
    { label: user_gateway_menu(), current: true },
  ]);
});

Deno.test("the agent turn history extends the agent chain", () => {
  assertEquals(matchTrail("/tagma/t-1/details/agents/a-9/history"), [
    { label: nav_home(), href: "/" },
    { label: nav_breadcrumb_tagma(), href: "/tagma/t-1/details" },
    {
      label: nav_breadcrumb_agents(),
      href: "/tagma/t-1/details/agents",
    },
    { label: "a-9", href: "/tagma/t-1/details/agents/a-9" },
    { label: manage_history_heading(), current: true },
  ]);
});

Deno.test("a discussion's page chains through its room", () => {
  assertEquals(matchTrail("/rooms/r-1/threads/7"), [
    { label: nav_home(), href: "/" },
    { label: nav_rooms(), href: "/rooms" },
    { label: room_label_fallback({ id: "r-1" }), href: "/rooms/r-1?at=7" },
    { label: room_thread_view(), current: true },
  ]);
});

Deno.test(
  "a discussion the window has read is named by its first line",
  async () => {
    const { rememberThreadHead } =
      await import("../rooms/threadHeads.svelte.ts");
    rememberThreadHead("r-2", 9, "the opening words");
    const trail = matchTrail("/rooms/r-2/threads/9");
    if (!trail) throw new Error("the discussion chain must resolve");
    assertEquals(trail.at(-1), { label: "the opening words", current: true });
    assertEquals(trail[2]?.href, "/rooms/r-2?at=9");
  },
);
