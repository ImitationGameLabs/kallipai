// Derive the nav link list from the app mode. The two modes are mutually
// exclusive front-door choices (see lib/config/mode.ts):
//   - online  -> Tagmata only (chat/approvals are not available until the agora
//     chat data-plane ships);
//   - offline -> Chat + Approvals (no tagmata; no identity).
// Settings is intentionally NOT a nav link: it lives in the AccountMenu
// dropdown (footer) alongside the other account/system actions.
// Icons are injected by the caller so this package does not depend on an icon
// library.
import type { Component } from "svelte";
import type { AppMode } from "../config/mode.ts";
import type { NavItem } from "../shell.ts";

export interface NavIcons {
  chat: Component;
  approvals: Component;
  tagmata: Component;
}

export function navFor(args: { mode: AppMode; icons: NavIcons }): NavItem[] {
  const { mode, icons } = args;
  if (mode === "offline") {
    return [
      { href: "/", label: "Chat", icon: icons.chat },
      { href: "/approvals", label: "Approvals", icon: icons.approvals },
    ];
  }
  return [{ href: "/tagmata", label: "Tagmata", icon: icons.tagmata }];
}
