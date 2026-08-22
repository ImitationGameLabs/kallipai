// Derive the nav link sections from the app mode. The two modes are mutually
// exclusive front-door choices (see lib/config/mode.ts):
//   - online  -> "Tagmata" and "Rooms" sections. Each section title carries a
//     settings gear to its management page -- /tagmata for the tagma registry,
//     /rooms for room management (create / invites / public rooms) -- so the
//     management surfaces are reached through the section header, not a flat
//     sibling link. "Tagmata" lists EVERY enrolled tagma (whether or not a
//     relay channel is currently open -- the link is always navigable and the
//     channel opens on demand at /chat/t/{tagmaId}); "Rooms" lists the caller's
//     rooms as direct chat entries (`/rooms/{id}`).
//   - offline -> Chat (the single local conversation at /local/chat), no header.
// Settings is intentionally NOT a nav link: it lives in the AccountMenu
// dropdown (footer) alongside the other account/system actions.
// Icons are injected by the caller so this package does not depend on an icon
// library.
import type { Component } from "svelte";
import type { AppMode } from "../config/mode.ts";
import type { NavIndicator, NavItem } from "../shell.ts";
import type { TagmaChannelState } from "../session/channels.svelte.ts";
import {
  nav_agents,
  nav_budget,
  nav_chat,
  nav_manage,
  nav_overview,
  nav_profiles,
  nav_room_management,
  nav_rooms,
  nav_schedules,
  nav_tagma_management,
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
  hub?: { href: string; label: string; icon: Component };
  /** The section's management page, reached via a settings gear beside the
   * title. `icon` is injected by the caller (mirrors NavItem). */
  manage?: { href: string; label: string; icon: Component };
  items: NavItem[];
}

export interface NavIcons {
  chat: Component;
  tagmata: Component;
  rooms: Component;
  /** Gear icon for the section-management entry beside each section title. */
  settings: Component;
  /** Management section icons (offline mode only). */
  /** The bottom-bar "manage" hub cell (offline mode only). */
  manageHub: Component;
  manageOverview: Component;
  manageBudget: Component;
  manageAgents: Component;
  manageProfiles: Component;
  manageSchedules: Component;
}

/** One enrolled tagma as a sidebar chat entry. `indicator` is the channel
 * transport status as a nav dot tri-state (the caller derives it from
 * `channelsStore.getTagmaChannelState` via `tagmaNavIndicator`). The entry is
 * always navigable -- the relay channel opens on demand at /chat/t/{tagmaId}. */
export interface NavTagma {
  tagmaId: string;
  label: string | null;
  indicator: NavIndicator;
}

/** One room as a sidebar chat entry (`/rooms/{id}`). Rooms have no live
 * transport status to dot, so they carry the rooms icon as their leading mark. */
export interface NavRoom {
  roomId: string;
  label: string;
}

/** Derive a sidebar NavIndicator from OUR channel transport state. Channel-
 *  transport-only (not peer presence): when the realtime SSE is broken, peer
 *  presence is unknown, so a presence-driven dot would mislabel every tagma
 *  "offline". This mapping stays honest about what we know:
 *    open      -> live (green)
 *    pending   -> pending (spinner; in-flight open or KEX)
 *    absent    -> pending (spinner; no channel yet -- click to connect)
 *    offline   -> down (grey; we had a channel and the peer went away)
 *    error     -> error (red; click to retry) */
export function tagmaNavIndicator(channel: TagmaChannelState): NavIndicator {
  switch (channel.kind) {
    case "open":
      return "live";
    case "pending":
    case "absent":
      return "pending";
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
}): NavSection[] {
  const { mode, icons, tagmata, rooms } = args;
  if (mode === "offline") {
    return [
      { items: [{ href: "/local/chat", label: nav_chat(), icon: icons.chat }] },
      {
        // `hub` folds this whole section into one small-screen bar cell;
        // the items stay for the desktop sidebar and the hub page itself.
        hub: {
          href: "/local/manage",
          label: nav_manage(),
          icon: icons.manageHub,
        },
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
        ],
      },
    ];
  }
  return [
    {
      title: nav_tagmata(),
      manage: {
        href: "/tagmata",
        label: nav_tagma_management(),
        icon: icons.settings,
      },
      items: (tagmata ?? []).map((t) => {
        // Each tagma uses a status dot (no icon) as the leading mark, so each
        // reads as its own destination under the tagma surface. The link is
        // always navigable; the channel opens on demand at the tagma route.
        return {
          href: `/chat/t/${t.tagmaId}`,
          label: t.label ?? tagma_profile_unnamed(),
          indicator: t.indicator,
        };
      }),
    },
    {
      title: nav_rooms(),
      manage: {
        href: "/rooms",
        label: nav_room_management(),
        icon: icons.settings,
      },
      items: (rooms ?? []).map((r) => ({
        href: `/rooms/${r.roomId}`,
        label: r.label,
        icon: icons.rooms,
      })),
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
