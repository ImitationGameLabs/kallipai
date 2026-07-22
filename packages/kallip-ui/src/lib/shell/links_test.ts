import { assertEquals } from "@std/assert";
import { navFor, pathMatches, type NavIcons } from "./links.ts";

// navFor only stores the icon components; dummies suffice.
const icons = {
  chat: () => {},
  approvals: () => {},
  tagmata: () => {},
} as unknown as NavIcons;

Deno.test("navFor online -> Tagmata only (Settings is in AccountMenu)", () => {
  const links = navFor({ mode: "online", icons });
  assertEquals(
    links.map((l) => l.href),
    ["/tagmata"],
  );
});

Deno.test("navFor online appends one entry per open channel", () => {
  const links = navFor({
    mode: "online",
    icons,
    channels: [
      { convId: "c1", label: "Laptop", indicator: "live" },
      { convId: "c2", label: null, indicator: "down" },
      { convId: "c3", label: "Phone", indicator: "pending" },
    ],
  });
  // Channels use an indicator dot, not an icon, so they read as their own
  // destinations distinct from the Tagmata management entry.
  assertEquals(
    links.map((l) => ({
      href: l.href,
      label: l.label,
      icon: !!l.icon,
      indicator: l.indicator ?? null,
    })),
    [
      { href: "/tagmata", label: "Tagmata", icon: true, indicator: null },
      { href: "/chat/c1", label: "Laptop", icon: false, indicator: "live" },
      {
        href: "/chat/c2",
        label: "Unnamed tagma",
        icon: false,
        indicator: "down",
      },
      { href: "/chat/c3", label: "Phone", icon: false, indicator: "pending" },
    ],
  );
});

Deno.test("navFor online with an empty channels array -> Tagmata only", () => {
  const links = navFor({ mode: "online", icons, channels: [] });
  assertEquals(
    links.map((l) => l.href),
    ["/tagmata"],
  );
});

Deno.test(
  "navFor offline -> Chat + Approvals (Settings is in AccountMenu)",
  () => {
    const links = navFor({ mode: "offline", icons });
    assertEquals(
      links.map((l) => l.href),
      ["/", "/approvals"],
    );
  },
);

Deno.test("pathMatches uses segment boundaries (no prefix cross-match)", () => {
  // Exact + beneath.
  assertEquals(pathMatches("/tagmata", "/tagmata"), true);
  assertEquals(pathMatches("/tagmata/x", "/tagmata"), true);
  // Root is exact-only.
  assertEquals(pathMatches("/", "/"), true);
  assertEquals(pathMatches("/chat", "/"), false);
  // Sibling /chat/{id} entries must NOT cross-highlight: /chat/ab is not under
  // /chat/a (no trailing slash boundary).
  assertEquals(pathMatches("/chat/ab", "/chat/a"), false);
  assertEquals(pathMatches("/chat/a", "/chat/a"), true);
  assertEquals(pathMatches("/chat/a/sub", "/chat/a"), true);
  // A non-matching prefix entirely.
  assertEquals(pathMatches("/approvals", "/tagmata"), false);
});
