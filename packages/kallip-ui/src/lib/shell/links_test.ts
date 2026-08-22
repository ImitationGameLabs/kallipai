import { assertEquals } from "@std/assert";
import {
  navFor,
  type NavIcons,
  pathMatches,
  tagmaNavIndicator,
} from "./links.ts";

// navFor only stores the icon components; dummies suffice.
const icons = {
  chat: () => {},
  tagmata: () => {},
  rooms: () => {},
  settings: () => {},
} as unknown as NavIcons;

// The section shape stripped to its structural parts (title + manage href +
// hub href + item hrefs) for the assertions below.
function shape(
  sections: ReturnType<typeof navFor>,
): {
  title: string | null;
  manage: string | null;
  hub: string | null;
  items: string[];
}[] {
  return sections.map((s) => ({
    title: s.title ?? null,
    manage: s.manage?.href ?? null,
    hub: s.hub?.href ?? null,
    items: s.items.map((l) => l.href),
  }));
}

Deno.test(
  "navFor online -> Tagmata + Rooms sections with a manage gear each",
  () => {
    const sections = navFor({ mode: "online", icons });
    assertEquals(shape(sections), [
      { title: "Tagmata", manage: "/tagmata", hub: null, items: [] },
      { title: "Rooms", manage: "/rooms", hub: null, items: [] },
    ]);
  },
);

Deno.test(
  "navFor online lists each room as a chat entry (management lives in the gear)",
  () => {
    const sections = navFor({
      mode: "online",
      icons,
      rooms: [
        { roomId: "r1", label: "Design" },
        { roomId: "r2", label: "Ops" },
      ],
    });
    assertEquals(shape(sections), [
      { title: "Tagmata", manage: "/tagmata", hub: null, items: [] },
      {
        title: "Rooms",
        manage: "/rooms",
        hub: null,
        items: ["/rooms/r1", "/rooms/r2"],
      },
    ]);
  },
);

Deno.test("navFor online lists every enrolled tagma under Tagmata", () => {
  const sections = navFor({
    mode: "online",
    icons,
    tagmata: [
      { tagmaId: "t1", label: "Laptop", indicator: "live" },
      { tagmaId: "t2", label: null, indicator: "down" },
      { tagmaId: "t3", label: "Phone", indicator: "pending" },
    ],
  });
  // Tagmas use an indicator dot, not an icon, and link to the tagma-keyed
  // route (always navigable -- the channel opens on demand there). An entry
  // appears regardless of whether its channel is open: live / down / pending
  // are all present, proving visibility is not gated on an open channel.
  assertEquals(
    sections.map((s) => ({
      title: s.title,
      manage: s.manage?.href ?? null,
      hub: s.hub?.href ?? null,
      items: s.items.map((l) => ({
        href: l.href,
        label: l.label,
        icon: !!l.icon,
        indicator: l.indicator ?? null,
      })),
    })),
    [
      {
        title: "Tagmata",
        manage: "/tagmata",
        hub: null,
        items: [
          {
            href: "/chat/t/t1",
            label: "Laptop",
            icon: false,
            indicator: "live",
          },
          {
            href: "/chat/t/t2",
            label: "Unnamed tagma",
            icon: false,
            indicator: "down",
          },
          {
            href: "/chat/t/t3",
            label: "Phone",
            icon: false,
            indicator: "pending",
          },
        ],
      },
      { title: "Rooms", manage: "/rooms", hub: null, items: [] },
    ],
  );
});

Deno.test(
  "navFor online Tagmata section is empty when no tagmata passed",
  () => {
    const sections = navFor({ mode: "online", icons });
    assertEquals(shape(sections), [
      { title: "Tagmata", manage: "/tagmata", hub: null, items: [] },
      { title: "Rooms", manage: "/rooms", hub: null, items: [] },
    ]);
  },
);

Deno.test("tagmaNavIndicator maps each channel state (transport-only)", () => {
  // open -> live; pending/absent -> pending (spinner, click to connect);
  // offline -> down; error -> error.
  assertEquals(
    tagmaNavIndicator({ kind: "open", conversationId: "c" }),
    "live",
  );
  assertEquals(
    tagmaNavIndicator({ kind: "pending", conversationId: "c" }),
    "pending",
  );
  assertEquals(tagmaNavIndicator({ kind: "absent" }), "pending");
  assertEquals(
    tagmaNavIndicator({ kind: "offline", conversationId: "c" }),
    "down",
  );
  assertEquals(
    tagmaNavIndicator({ kind: "error", conversationId: "c" }),
    "error",
  );
});

Deno.test("navFor offline -> Chat + Manage sections", () => {
  const sections = navFor({ mode: "offline", icons });
  assertEquals(shape(sections), [
    { title: null, manage: null, hub: null, items: ["/local/chat"] },
    {
      title: "Manage",
      manage: null,
      hub: "/local/manage",
      items: [
        "/local/manage/overview",
        "/local/manage/budget",
        "/local/manage/agents",
        "/local/manage/profiles",
        "/local/manage/schedules",
      ],
    },
  ]);
});

Deno.test("pathMatches uses segment boundaries (no prefix cross-match)", () => {
  // Exact + beneath.
  assertEquals(pathMatches("/rooms", "/rooms"), true);
  assertEquals(pathMatches("/rooms/x", "/rooms"), true);
  // Root is exact-only.
  assertEquals(pathMatches("/", "/"), true);
  assertEquals(pathMatches("/chat", "/"), false);
  // Sibling /chat/{id} entries must NOT cross-highlight: /chat/ab is not under
  // /chat/a (no trailing slash boundary).
  assertEquals(pathMatches("/chat/ab", "/chat/a"), false);
  assertEquals(pathMatches("/chat/a", "/chat/a"), true);
  assertEquals(pathMatches("/chat/a/sub", "/chat/a"), true);
  // The tagma-keyed route /chat/t/{tagmaId} matches itself exactly, and does
  // not cross-highlight with a sibling /chat/{conversationId} entry.
  assertEquals(pathMatches("/chat/t/abc", "/chat/t/abc"), true);
  assertEquals(pathMatches("/chat/t/abc", "/chat/abc"), false);
  assertEquals(pathMatches("/chat/abc", "/chat/t/abc"), false);
  // A non-matching prefix entirely.
  assertEquals(pathMatches("/approvals", "/tagmata"), false);
});
