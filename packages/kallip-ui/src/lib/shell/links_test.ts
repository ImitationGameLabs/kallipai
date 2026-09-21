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
  home: () => {},
  files: () => {},
} as unknown as NavIcons;

// The section shape stripped to its structural parts (title + manage href +
// hub href + smallScreenHidden + item hrefs) for the assertions below.
function shape(sections: ReturnType<typeof navFor>): {
  title: string | null;
  manage: string | null;
  hub: string | null;
  smallScreenHidden: boolean;
  items: string[];
}[] {
  return sections.map((s) => ({
    title: s.title ?? null,
    manage: s.manage?.href ?? null,
    hub: s.hub?.href ?? null,
    smallScreenHidden: s.smallScreenHidden === true,
    items: s.items.map((l) => l.href),
  }));
}

Deno.test("navFor online -> Chats hub section + Manage cell (no gears)", () => {
  const sections = navFor({ mode: "online", icons });
  assertEquals(shape(sections), [
    {
      title: "Chats",
      manage: null,
      hub: "/chats",
      smallScreenHidden: false,
      items: [],
    },
    {
      title: "Tagmata",
      manage: null,
      hub: null,
      smallScreenHidden: false,
      items: ["/tagmata", "/files"],
    },
  ]);
});

Deno.test(
  "navFor online lists each room in the Chats hub section (rooms icons)",
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
      {
        title: "Chats",
        manage: null,
        hub: "/chats",
        smallScreenHidden: false,
        items: ["/rooms/r1", "/rooms/r2"],
      },
      {
        title: "Tagmata",
        manage: null,
        hub: null,
        smallScreenHidden: false,
        items: ["/tagmata", "/files"],
      },
    ]);
  },
);

Deno.test("navFor online lists every enrolled tagma in the hub section", () => {
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
  // The trailing cells are the combined manage page (labeled by the
  // action) and the files page.
  assertEquals(
    sections.map((s) => ({
      title: s.title,
      manage: s.manage?.href ?? null,
      hub: s.hub?.href ?? null,
      smallScreenHidden: s.smallScreenHidden === true,
      items: s.items.map((l) => ({
        href: l.href,
        label: l.label,
        icon: !!l.icon,
        indicator: l.indicator ?? null,
      })),
    })),
    [
      {
        title: "Chats",
        manage: null,
        hub: "/chats",
        smallScreenHidden: false,
        items: [
          {
            href: "/tagma/t1/chat",
            label: "Laptop",
            icon: false,
            indicator: "live",
          },
          {
            href: "/tagma/t2/chat",
            label: "Unnamed tagma",
            icon: false,
            indicator: "down",
          },
          {
            href: "/tagma/t3/chat",
            label: "Phone",
            icon: false,
            indicator: "pending",
          },
        ],
      },
      {
        title: "Tagmata",
        manage: null,
        hub: null,
        smallScreenHidden: false,
        items: [
          {
            href: "/tagmata",
            label: "Manage",
            icon: true,
            indicator: null,
          },
          {
            href: "/files",
            label: "Files",
            icon: true,
            indicator: null,
          },
        ],
      },
    ],
  );
});

Deno.test(
  "navFor online merged ordering: tagma chats first, rooms after",
  () => {
    const sections = navFor({
      mode: "online",
      icons,
      tagmata: [{ tagmaId: "t1", label: "Laptop", indicator: "live" }],
      rooms: [{ roomId: "r1", label: "Design" }],
    });
    assertEquals(shape(sections), [
      {
        title: "Chats",
        manage: null,
        hub: "/chats",
        smallScreenHidden: false,
        items: ["/tagma/t1/chat", "/rooms/r1"],
      },
      {
        title: "Tagmata",
        manage: null,
        hub: null,
        smallScreenHidden: false,
        items: ["/tagmata", "/files"],
      },
    ]);
  },
);

Deno.test("tagmaNavIndicator maps each channel state", () => {
  // open -> live while the peer reads online (or presence is unresolved);
  // down once presence resolves without the peer (knownOffline) -- a
  // stopped peer cannot decrypt under our session key;
  assertEquals(
    tagmaNavIndicator({ kind: "open", conversationId: "c" }, false),
    "live",
  );
  assertEquals(
    tagmaNavIndicator({ kind: "open", conversationId: "c" }, true),
    "down",
  );
  assertEquals(
    tagmaNavIndicator({ kind: "pending", conversationId: "c" }),
    "pending",
  );
  // presence must not bleed beyond open and absent: the in-flight arm
  // ignores it, so a pending open stays pending even once the peer
  // reads offline;
  assertEquals(
    tagmaNavIndicator({ kind: "pending", conversationId: "c" }, true),
    "pending",
  );
  assertEquals(tagmaNavIndicator({ kind: "absent" }), "pending");
  assertEquals(tagmaNavIndicator({ kind: "absent" }, false), "pending");
  assertEquals(tagmaNavIndicator({ kind: "absent" }, true), "down");
  assertEquals(tagmaNavIndicator({ kind: "unavailable" }), "down");
  assertEquals(tagmaNavIndicator({ kind: "unavailable" }, true), "down");
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
    {
      title: null,
      manage: null,
      hub: "/local",
      smallScreenHidden: false,
      items: ["/local/chat"],
    },
    {
      title: "Manage",
      manage: null,
      hub: null,
      smallScreenHidden: true,
      items: [
        "/local/manage/overview",
        "/local/manage/budget",
        "/local/manage/agents",
        "/local/manage/profiles",
        "/local/manage/schedules",
        "/local/manage/tasks",
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
  // The tagma-keyed route /tagma/{tagmaId}/chat matches itself exactly, and
  // does not cross-highlight with a sibling /chat/{conversationId} entry.
  assertEquals(pathMatches("/chat/t/abc", "/chat/t/abc"), true);
  assertEquals(pathMatches("/chat/t/abc", "/chat/abc"), false);
  assertEquals(pathMatches("/tagma/abc/chat", "/chat/abc"), false);
  assertEquals(pathMatches("/chat/abc", "/tagma/abc/chat"), false);
  assertEquals(pathMatches("/chat/abc", "/chat/t/abc"), false);
  // The details tree follows the same rules: hub and sibling sections do
  // not cross-highlight, and the old /chat/t/... manage shape never
  // matches the new tree (the shapes coexist only via redirects).
  assertEquals(pathMatches("/tagma/abc/details", "/tagma/abc/details"), true);
  assertEquals(
    pathMatches("/tagma/abc/details/overview", "/tagma/abc/details"),
    true,
  );
  assertEquals(
    pathMatches("/tagma/abc/details/overview", "/tagma/abc/details/budget"),
    false,
  );
  assertEquals(pathMatches("/tagma/abc/details", "/tagma/abc/chat"), false);
  assertEquals(pathMatches("/tagma/abc/chat", "/tagma/abc/details"), false);
  assertEquals(
    pathMatches("/chat/t/abc/manage/overview", "/tagma/abc/details"),
    false,
  );
  assertEquals(
    pathMatches("/tagma/abc/details/overview", "/chat/t/abc/manage"),
    false,
  );
  // A non-matching prefix entirely.
  assertEquals(pathMatches("/approvals", "/tagmata"), false);
});
