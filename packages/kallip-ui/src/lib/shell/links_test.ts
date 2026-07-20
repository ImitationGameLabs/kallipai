import { assertEquals } from "@std/assert";
import { navFor, type NavIcons } from "./links.ts";

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
