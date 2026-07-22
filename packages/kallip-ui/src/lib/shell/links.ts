// Derive the nav link list from the app mode. The two modes are mutually
// exclusive front-door choices (see lib/config/mode.ts):
//   - online  -> Tagmata, plus one entry per open herald channel (online chat);
//   - offline -> Chat + Approvals (no tagmata; no identity).
// Settings is intentionally NOT a nav link: it lives in the AccountMenu
// dropdown (footer) alongside the other account/system actions.
// Icons are injected by the caller so this package does not depend on an icon
// library.
import type { Component } from "svelte";
import type { AppMode } from "../config/mode.ts";
import type { NavIndicator, NavItem } from "../shell.ts";

export interface NavIcons {
  chat: Component;
  approvals: Component;
  tagmata: Component;
}

/** A summary of one open channel, for the sidebar nav. `indicator` is the
 * channel's transport status as a nav dot tri-state (the store maps its
 * ChannelState.status to this). */
export interface NavChannel {
  convId: string;
  label: string | null;
  indicator: NavIndicator;
}

export function navFor(args: {
  mode: AppMode;
  icons: NavIcons;
  channels?: NavChannel[];
}): NavItem[] {
  const { mode, icons, channels } = args;
  if (mode === "offline") {
    return [
      { href: "/", label: "Chat", icon: icons.chat },
      { href: "/approvals", label: "Approvals", icon: icons.approvals },
    ];
  }
  const links: NavItem[] = [
    { href: "/tagmata", label: "Tagmata", icon: icons.tagmata },
  ];
  if (channels) {
    for (const c of channels) {
      // Open chats use a status dot (no icon) as the leading mark, so each
      // channel reads as its own destination, distinct from the Tagmata entry.
      links.push({
        href: `/chat/${c.convId}`,
        label: c.label ?? "Unnamed tagma",
        indicator: c.indicator,
      });
    }
  }
  return links;
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
