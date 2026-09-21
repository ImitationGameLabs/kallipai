/**
 * Single source for the shell breadcrumb trail: a route table keyed by
 * pathname pattern, rendered by the desktop shell's one chrome bar. A
 * shell page gets
 * its trail by adding a table entry, not by mounting a component -- so a new
 * page cannot forget the trail or drift from the chrome (the shell owns
 * divider, height, and segment style). A route with no entry renders no bar
 * at all, which keeps the offline /local/* tree bar-free; the one deliberate
 * tabled exception to the shell-only rule is the agent detail route below,
 *
 * Resolvers run inside the shell's $derived, so store reads (room names,
 * channel labels) stay reactive while a deep view loads. The matching engine
 * itself lives in trailMatch.ts, dependency-free and unit-tested there.
 */
import { archeionSession } from "../session/archeion.svelte.ts";
import { channelsStore } from "../session/channels.svelte.ts";
import { roomsStore } from "../session/rooms.svelte.ts";
import { RelayConversation } from "../session/conversation.svelte.ts";
import { directSessionsStore } from "../session/directSessions.svelte.ts";
import {
  tagmaDetailsPath,
  tagmaDetailsSectionPath,
  type TagmaDetailsSection,
} from "./routes.ts";
import {
  account_menu,
  chat_title_local,
  nav_budget,
  nav_breadcrumb_agents,
  nav_breadcrumb_tagma,
  nav_chats,
  nav_files,
  nav_home,
  nav_overview,
  nav_profiles,
  nav_rooms,
  nav_schedules,
  nav_tasks,
  nav_tagmata,
  room_label_fallback,
  settings_heading,
  tagma_fallback_label,
} from "../../paraglide/messages.js";
import {
  entry,
  matchTrail as matchTable,
  backFromTrail,
  type TrailEntry,
  type With,
} from "./trailMatch.ts";

// Re-exported so the shell (and any future consumer) can treat this module
// as the single import surface for the trail: the table plus the matcher.
export {
  type BreadcrumbSegment,
  type TrailEntry,
  type TrailParams,
  type With,
} from "./trailMatch.ts";

// The details sections beneath the tagma hub, labeled exactly as the
// OnlineManagePage headings (nav_breadcrumb_agents, not nav_agents: the
// trail word is "agents" in the breadcrumb register).
const sectionLabels: Record<TagmaDetailsSection, () => string> = {
  overview: nav_overview,
  budget: nav_budget,
  agents: nav_breadcrumb_agents,
  profiles: nav_profiles,
  schedules: nav_schedules,
  tasks: nav_tasks,
};

// Operator-set IA rule: every entry opens with the Home segment (target
// /) and the middle layers follow the real hierarchy (domain hub,
// object, subpage). Online "/" itself stays off-table (the panorama is
// the root: no parent to fall back to); offline "/" falls to /local via
// the gate.
export const trailTable: TrailEntry[] = [
  entry("/tagmata", () => [
    { label: nav_home(), href: "/" },
    { label: nav_tagmata(), current: true },
  ]),
  entry("/files", () => [
    { label: nav_home(), href: "/" },
    { label: nav_files(), current: true },
  ]),
  entry("/tagma/:id", ({ id }: With<"id">) => [
    { label: nav_home(), href: "/" },
    { label: nav_tagmata(), href: "/tagmata" },
    {
      label:
        archeionSession.enrolledCards.find((t) => t.tagmaId === id)?.label ??
        tagma_fallback_label({ id: id.slice(0, 8) }),
      current: true,
    },
  ]),
  entry("/tagma/:id/chat", ({ id }: With<"id">) => [
    { label: nav_home(), href: "/" },
    { label: nav_chats(), href: "/chats" },
    {
      label:
        archeionSession.enrolledCards.find((t) => t.tagmaId === id)?.label ??
        tagma_fallback_label({ id: id.slice(0, 8) }),
      current: true,
    },
  ]),
  entry(
    "/tagma/:id/details/:section",
    ({ id, section }: With<"id" | "section">) => [
      { label: nav_home(), href: "/" },
      { label: nav_breadcrumb_tagma(), href: tagmaDetailsPath(id) },
      { label: sectionLabels[section as TagmaDetailsSection](), current: true },
    ],
  ),
  entry(
    "/tagma/:id/details/agents/:agentId",
    ({ id, agentId }: With<"id" | "agentId">) => [
      { label: nav_home(), href: "/" },
      { label: nav_breadcrumb_tagma(), href: tagmaDetailsPath(id) },
      {
        label: nav_breadcrumb_agents(),
        href: tagmaDetailsSectionPath(id, "agents"),
      },
      // The agent's role lives in the page's own backend (which keeps this
      // route off the global store), so the trail tail carries the id prefix
      // and the page header carries the role -- the same fallback branch the
      // page itself used before the trail moved here.
      { label: agentId.slice(0, 8), current: true },
    ],
  ),
  entry("/rooms", () => [
    { label: nav_home(), href: "/" },
    { label: nav_rooms(), current: true },
  ]),
  // Drill chains extend the list page's chain verbatim and append:
  // following a crumb must not reshape the bar. Every entry also opens
  // with Home (target /) -- the bar's universal first crumb.
  entry("/rooms/:id", ({ id }: With<"id">) => {
    const name = roomsStore.rooms.find((r) => r.room_id === id)?.name;
    return [
      { label: nav_home(), href: "/" },
      { label: nav_rooms(), href: "/rooms" },
      {
        label: name || room_label_fallback({ id: id.slice(0, 8) }),
        current: true,
      },
    ];
  }),
  entry("/rooms/:id/settings", ({ id }: With<"id">) => {
    const name = roomsStore.rooms.find((r) => r.room_id === id)?.name;
    return [
      { label: nav_home(), href: "/" },
      { label: nav_rooms(), href: "/rooms" },
      {
        label: name || room_label_fallback({ id: id.slice(0, 8) }),
        href: `/rooms/${id}`,
      },
      { label: settings_heading(), current: true },
    ];
  }),
  entry("/chat/:id", ({ id }: With<"id">) => {
    const conv = channelsStore.get(id);
    const label =
      conv instanceof RelayConversation && conv.label !== null
        ? conv.label
        : id === "local"
          ? chat_title_local()
          : id.slice(0, 8);
    return [
      { label: nav_home(), href: "/" },
      { label: nav_chats(), href: "/chats" },
      { label, current: true },
    ];
  }),
  // The direct-session transcript: chat-domain (the entering section is the
  // chats hub, which the trail chain yields as the mobile back target).
  entry("/tagma/:id/direct/:peer", ({ peer }: With<"id" | "peer">) => [
    { label: nav_home(), href: "/" },
    { label: nav_chats(), href: "/chats" },
    // `id` (the fetch-through daemon) is deliberately not in the label:
    // the peer is what the breadcrumb names.
    { label: directSessionsStore.peerLabel(peer), current: true },
  ]),
  entry("/settings", () => [
    { label: nav_home(), href: "/" },
    { label: settings_heading(), current: true },
  ]),
  entry("/account", () => [
    { label: nav_home(), href: "/" },
    { label: account_menu(), current: true },
  ]),
  // The matcher decodes captures once, aligned with the decoded params the
  // framework hands the page shells -- the resolver uses them as-is.
  entry("/user/:handle", ({ handle }: With<"handle">) => [
    { label: nav_home(), href: "/" },
    { label: handle, current: true },
  ]),
];

/** Match a pathname against the route table; the first entry whose pattern
 * matches segment-for-segment wins. Returns null for untabled routes (the
 * shell renders no bar there). */
export function matchTrail(pathname: string) {
  return matchTable(trailTable, pathname);
}

/** The small-screen back row's target: trail-derived (the deepest linked
 * segment is the parent, the same chain the desktop bar renders), with
 * the route policy the pure engine must not own. Bar-cell destinations
 * (/account, /tagmata, /files) are excluded: swapping the bar for a back
 * there would strand the other cells. /chats stays off-table and keeps
 * the bar with no exclusion needed. The tagma details sections return
 * null and keep the bar (the manage cell lights via RootLayout
 * isActive); the agent detail below them stays a drill (back = its
 * agents section), and so does the tagma's own hub (/tagma/{id}). */
export function mobileBack(
  pathname: string,
): { href: string; label: string } | null {
  if (
    pathname === "/account" ||
    pathname === "/tagmata" ||
    pathname === "/files"
  )
    return null;
  const segs = pathname.split("/").filter(Boolean);
  if (segs.length === 4 && segs[0] === "tagma" && segs[2] === "details") {
    return null;
  }
  return backFromTrail(matchTrail(pathname));
}
