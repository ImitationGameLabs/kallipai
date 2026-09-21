// Derive the nav link sections from the app mode. The two modes are mutually
// exclusive front-door choices (see lib/config/mode.ts):
//   - online  -> a merged "Chats" section: every enrolled tagma chat (whether
//     or not a relay channel is open -- the link is always navigable and the
//     channel opens on demand at /tagma/{tagmaId}/chat) followed by the
//     caller's rooms as direct chat entries (/rooms/{id}); the two groups are
//     told apart by their leading mark (status dot vs rooms icon). The section
//     declares hub -> /chats, so the bottom bar folds it into one cell and the
//     /chats hub page carries the grouped view as pure conversation rows. A
//     second two-item "Manage" section carries the combined manage page
//     (/tagmata: Tagmata registry + Rooms management) and the files page
//     (/files), both reached directly.
//   - offline -> Chat (the single local conversation at /local/chat), no header.
// Settings is intentionally NOT a nav link: it lives in the AccountMenu
// dropdown (footer) alongside the other account/system actions.
// Icons are injected by the caller so this package does not depend on an icon
// library.
import type { Component } from "svelte";
import type { AppMode } from "../config/mode.ts";
import type { NavIndicator, NavItem } from "../shell.ts";
import { filesPath, tagmaChatPath } from "./routes.ts";
import type { TagmaChannelState } from "../session/channels.svelte.ts";
import {
  nav_agents,
  nav_budget,
  nav_chat,
  nav_files,
  nav_home,
  nav_manage,
  nav_overview,
  nav_profiles,
  nav_chats,
  nav_schedules,
  nav_tasks,
  nav_tagmata,
  tagma_profile_unnamed,
} from "../../paraglide/messages.js";

// One sidebar section. `title` renders as a small header with a divider
// beneath it; `manage` renders as a settings gear beside the title, linking to
// the section's management page. An untitled section (offline mode's single
// Chat entry) renders its items bare. `hub` is the small-screen-only twin of a
// titled items list: navSlots folds such a section into one bottom-bar cell
// whose five items live behind the hub page instead (the desktop sidebar and
// the More sheet both ignore it).
export interface NavSection {
  title?: string;
  /** Small-screen single entry for this section's items; the bar renders it
   * as one cell instead of the items (which the hub page lists). */
  hub?: { href: string; label: string; icon: Component; badge?: number };
  /** The section's management page, reached via a settings gear beside the
   * title. `icon` is injected by the caller (mirrors NavItem). */
  manage?: { href: string; label: string; icon: Component };
  /** Small-screen-only kill switch: navSlots drops the section from the
   * bottom bar and the More sheet entirely; the desktop sidebar (which
   * renders sections directly) still shows it.
   */
  smallScreenHidden?: boolean;
  items: NavItem[];
}

export interface NavIcons {
  chat: Component;
  tagmata: Component;
  rooms: Component;
  /** Gear icon for the section-management entry beside each section title. */
  settings: Component;
  /** Management section icons (offline mode only). */
  /** The bottom-bar "home" hub cell (offline mode only). */
  home: Component;
  manageOverview: Component;
  manageBudget: Component;
  manageAgents: Component;
  manageProfiles: Component;
  manageSchedules: Component;
  manageTasks: Component;
  /** The files page entry (online Manage section, second item). */
  files: Component;
}

/** One enrolled tagma as a sidebar chat entry. `indicator` is the channel
 * transport status as a nav dot four-state (the caller derives it from
 * `channelsStore.getTagmaChannelState` via `tagmaNavIndicator`). The entry is
 * always navigable -- the relay channel opens on demand at the
 * /tagma/{tagmaId}/chat route. */
export interface NavTagma {
  tagmaId: string;
  label: string | null;
  indicator: NavIndicator;
  /** The unread count for this chat (rendered through badgeLabel). */
  badge?: number;
}

/** One room as a sidebar chat entry (`/rooms/{id}`). Rooms have no live
 * transport status to dot, so they carry the rooms icon as their leading mark. */
export interface NavRoom {
  roomId: string;
  label: string;
  /** The unread count for this room (rendered through badgeLabel). */
  badge?: number;
}

/** One direct session (agent↔agent on the relay) as a sidebar chat entry:
 *  /tagma/{tagmaId}/direct/{peerId}. The fetch-through daemon is the
 *  canonical min side; the label resolves through the store. */
export interface NavDirect {
  tagmaId: string;
  peerId: string;
  label: string;
}

/** Derive a sidebar NavIndicator from OUR channel transport state.
 *  Channel-transport-first: when the realtime SSE is broken, presence is
 *  unknown, so a presence-driven dot would mislabel every tagma "offline".
 *  Presence feeds exactly two branches: `open` and `absent` read down
 *  with `knownOffline`: a stopped peer silently drops our envelopes
 *  under the session key it forgot, so an open channel alone must not
 *  read live (the same safe-default policy as the /tagmata dashboard):
 *    open      -> live (green); down (grey) once presence resolves
 *    without the peer
 *    pending   -> pending (spinner; in-flight open or KEX)
 *    absent    -> pending (spinner) while presence is unresolved; down
 *    (grey) once presence resolves without the peer -- no channel exists
 *    and auto-open only fires for online tagmas
 *    unavailable -> down (grey; the auto-open budget holds a failure --
 *    nothing in flight; the chat page shows its unavailable + retry row)
 *    offline   -> down (grey; we had a channel and the peer went away)
 *    error     -> error (red; click to retry) */
export function tagmaNavIndicator(
  channel: TagmaChannelState,
  knownOffline = false,
): NavIndicator {
  switch (channel.kind) {
    case "open":
      // A live drain is not sendability: a stopped peer silently drops
      // envelopes under the session key it forgot, so trust presence
      // over the channel when the two disagree (absent mirrors this).
      return knownOffline ? "down" : "live";
    case "pending":
      return "pending";
    case "absent":
      // No channel and none in flight: auto-open only fires for online
      // tagmas, so an absent channel to a presence-confirmed-offline peer
      // would otherwise spin forever; unresolved presence keeps the spinner
      // (bounded by the realtime resolve deadline).
      return knownOffline ? "down" : "pending";
    case "unavailable":
      return "down";
    case "offline":
      return "down";
    case "error":
      return "error";
  }
}

export function navFor(args: {
  mode: AppMode;
  icons: NavIcons;
  tagmata?: NavTagma[];
  rooms?: NavRoom[];
  directs?: NavDirect[];
  /** The chats total for the small-screen hub cell (the Chats bar badge). */
  chatsBadge?: number;
}): NavSection[] {
  const { mode, icons, tagmata, rooms, directs } = args;
  if (mode === "offline") {
    return [
      {
        // `hub` folds this section into the small-screen "home" bar cell at
        // /local; the chat item below stays for the desktop sidebar. The
        // home page itself (LocalHomePage) carries the chat CTA and the
        // manage grid, so the bar never needs more than home + account.
        hub: { href: "/local", label: nav_home(), icon: icons.home },
        items: [{ href: "/local/chat", label: nav_chat(), icon: icons.chat }],
      },
      {
        // smallScreenHidden: desktop-sidebar-only on small viewports — its
        // entries live on the /local home grid instead of the bottom bar.
        smallScreenHidden: true,
        title: nav_manage(),
        items: [
          {
            href: "/local/manage/overview",
            label: nav_overview(),
            icon: icons.manageOverview,
          },
          {
            href: "/local/manage/budget",
            label: nav_budget(),
            icon: icons.manageBudget,
          },
          {
            href: "/local/manage/agents",
            label: nav_agents(),
            icon: icons.manageAgents,
          },
          {
            href: "/local/manage/profiles",
            label: nav_profiles(),
            icon: icons.manageProfiles,
          },
          {
            href: "/local/manage/schedules",
            label: nav_schedules(),
            icon: icons.manageSchedules,
          },
          {
            href: "/local/manage/tasks",
            label: nav_tasks(),
            icon: icons.manageTasks,
          },
        ],
      },
    ];
  }
  return [
    {
      // Merged chats hub: tagma rows (status dots) then room rows (rooms
      // icons) in one section -- the leading mark tells the groups apart.
      // `hub` folds it into the small-screen Chats bar cell; /chats lists
      // both groups as pure conversation rows -- no manage chip. Rooms
      // management lives on the combined /tagmata page (the withdrawn
      // chip/AccountMenu ideas were never shipped).
      title: nav_chats(),
      hub: {
        href: "/chats",
        label: nav_chats(),
        icon: icons.chat,
        badge: args.chatsBadge,
      },
      items: [
        ...(tagmata ?? []).map((t) => ({
          href: tagmaChatPath(t.tagmaId),
          label: t.label ?? tagma_profile_unnamed(),
          indicator: t.indicator,
          badge: t.badge,
        })),
        ...(rooms ?? []).map((r) => ({
          href: `/rooms/${r.roomId}`,
          label: r.label,
          badge: r.badge,
          icon: icons.rooms,
        })),
        ...(directs ?? []).map((d) => ({
          href: `/tagma/${d.tagmaId}/direct/${d.peerId}`,
          label: d.label,
          icon: icons.rooms,
        })),
      ],
    },
    {
      // The combined manage page and the user-scope files page as direct
      // bar cells (Tagmata registry + Rooms management sections, then
      // /files); the labels name the action, not the registry.
      title: nav_tagmata(),
      items: [
        { href: "/tagmata", label: nav_manage(), icon: icons.tagmata },
        { href: filesPath(), label: nav_files(), icon: icons.files },
      ],
    },
  ];
}

/** Segment-boundary route match: `href` is active when `pathname` is exactly it
 * or a path beneath it. A plain prefix test (`startsWith`) would let `/chat/ab`
 * wrongly match `/chat/a`; the trailing-slash rule prevents that, which matters
 * now that multiple `/chat/{id}` entries coexist in the sidebar. `"/"` is
 * matched exactly (no trailing-segment beneath root). */
export function pathMatches(pathname: string, href: string): boolean {
  if (href === "/") return pathname === "/";
  return pathname === href || pathname.startsWith(href + "/");
}
