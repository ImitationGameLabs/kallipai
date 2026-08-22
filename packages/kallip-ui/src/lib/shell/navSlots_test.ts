// Table-driven slot-plan tests: the cap arithmetic across both modes and the
// boundary item counts (0/1/2/3/4/6). Section/manage fixtures mirror the
// shapes navFor emits (links.ts): offline = an untitled chat section plus a
// geared-hub manage section (untitled raw-item sections cover the defensive
// arithmetic); online = two geared sections with N+M items.
import { assertEquals } from "@std/assert";
import type { NavItem } from "../shell.ts";
import type { NavSection } from "./links.ts";
import { navSlots } from "./navSlots.ts";

const item = (href: string): NavItem => ({ href, label: href });

function offlineSections(count: number): NavSection[] {
  return [{ items: Array.from({ length: count }, (_, i) => item(`/o${i}`)) }];
}

function onlineSections(n: number, m: number): NavSection[] {
  return [
    {
      title: "Tagmata",
      manage: { href: "/tagmata", label: "manage", icon: (() => {}) as never },
      items: Array.from({ length: n }, (_, i) => item(`/t/${i}`)),
    },
    {
      title: "Rooms",
      manage: { href: "/rooms", label: "manage", icon: (() => {}) as never },
      items: Array.from({ length: m }, (_, i) => item(`/r/${i}`)),
    },
  ];
}

const hrefs = (items: NavItem[]) => items.map((i) => i.href);

const cases: {
  name: string;
  links: NavSection[];
  visible: string[];
  overflow: string[];
  hasMore: boolean;
  sheetSections: {
    title: string | null;
    manage: string | null;
    items: string[];
  }[];
}[] = [
  {
    name: "offline 6 -> cap 3 + More + Account (5 bar cells)",
    links: offlineSections(6),
    visible: ["/o0", "/o1", "/o2"],
    overflow: ["/o3", "/o4", "/o5"],
    hasMore: true,
    sheetSections: [
      { title: null, manage: null, items: ["/o3", "/o4", "/o5"] },
    ],
  },
  {
    name:
      "offline 4 (defensive, structurally unreachable) -> all visible, no More",
    links: offlineSections(4),
    visible: ["/o0", "/o1", "/o2", "/o3"],
    overflow: [],
    hasMore: false,
    sheetSections: [],
  },
  {
    name: "offline hub -> manage folds to one cell; no More, sheet stays empty",
    links: [
      { items: [item("/local/chat")] },
      {
        title: "管理",
        hub: {
          href: "/local/manage",
          label: "管理",
          icon: (() => {}) as never,
        },
        items: offlineSections(6)[0].items,
      },
    ],
    visible: ["/local/chat", "/local/manage"],
    overflow: [],
    hasMore: false,
    sheetSections: [],
  },
  {
    name: "online 0+0 -> More ever-shown; sheet holds only geared headers",
    links: onlineSections(0, 0),
    visible: [],
    overflow: [],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: [] },
      { title: "Rooms", manage: "/rooms", items: [] },
    ],
  },
  {
    name: "online 1+0 -> 1 visible + More + Account",
    links: onlineSections(1, 0),
    visible: ["/t/0"],
    overflow: [],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: [] },
      { title: "Rooms", manage: "/rooms", items: [] },
    ],
  },
  {
    name: "online 2+2 -> items=4 caps at 3; the cut can fall mid-section",
    links: onlineSections(2, 2),
    visible: ["/t/0", "/t/1", "/r/0"],
    overflow: ["/r/1"],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: [] },
      { title: "Rooms", manage: "/rooms", items: ["/r/1"] },
    ],
  },
  {
    name: "online 2+0 -> items=2 all visible alongside More",
    links: onlineSections(2, 0),
    visible: ["/t/0", "/t/1"],
    overflow: [],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: [] },
      { title: "Rooms", manage: "/rooms", items: [] },
    ],
  },
  {
    name: "online 3+0 -> items=3 fills the cap exactly",
    links: onlineSections(3, 0),
    visible: ["/t/0", "/t/1", "/t/2"],
    overflow: [],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: [] },
      { title: "Rooms", manage: "/rooms", items: [] },
    ],
  },
  {
    name: "online 6+6 -> cap 3; both sections contribute overflow",
    links: onlineSections(6, 6),
    visible: ["/t/0", "/t/1", "/t/2"],
    overflow: [
      "/t/3",
      "/t/4",
      "/t/5",
      "/r/0",
      "/r/1",
      "/r/2",
      "/r/3",
      "/r/4",
      "/r/5",
    ],
    hasMore: true,
    sheetSections: [
      { title: "Tagmata", manage: "/tagmata", items: ["/t/3", "/t/4", "/t/5"] },
      {
        title: "Rooms",
        manage: "/rooms",
        items: ["/r/0", "/r/1", "/r/2", "/r/3", "/r/4", "/r/5"],
      },
    ],
  },
];

for (const c of cases) {
  Deno.test(`navSlots: ${c.name}`, () => {
    const plan = navSlots(c.links);
    assertEquals(hrefs(plan.visible), c.visible);
    assertEquals(hrefs(plan.overflow), c.overflow);
    assertEquals(plan.hasMore, c.hasMore);
    assertEquals(
      plan.sheetSections.map((s) => ({
        title: s.title ?? null,
        manage: s.manage?.href ?? null,
        items: hrefs(s.items),
      })),
      c.sheetSections,
    );
  });
}
