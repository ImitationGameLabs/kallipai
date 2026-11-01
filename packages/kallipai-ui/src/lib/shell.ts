// Types for the shared app shell. Kept in a plain `.ts` module (not inside
// the shell components) so consumers can import `NavItem` for type-only use.
import type { Component } from "svelte";

import {
  shell_connecting,
  shell_error,
  shell_live,
  shell_offline,
} from "../paraglide/messages.js";

// The indicator's visual tokens (dot classes + SR label) live HERE, not in
// the nav cells (NavLink): the shells' nav trees and hub rows (HubRow)
// render the same status dot, so the mapping must not fork.
export function navIndicatorDotClass(indicator: NavIndicator): string {
  switch (indicator) {
    case "live":
      return "bg-success-500";
    case "down":
      return "bg-surface-400-600";
    case "error":
      return "bg-error-500";
    // Unreachable at runtime: callers render a spinner for "pending"
    // before reaching here. Kept so the switch stays exhaustive.
    case "pending":
      return "bg-surface-400-600";
  }
}

// The dot itself is aria-hidden (decorative); this label carries the status
// to screen readers so an SR user learns the channel's liveness, not just
// its name.
export function navIndicatorLabel(indicator: NavIndicator): string {
  switch (indicator) {
    case "live":
      return shell_live();
    case "pending":
      return shell_connecting();
    case "down":
      return shell_offline();
    case "error":
      return shell_error();
  }
}

/** A small status indicator that nav cells (NavLink) render as a leading
 * dot instead of an icon (e.g. per-chat liveness in the sidebar). The
 * visual tokens live in this module (see above); consumers map their
 * domain state to the four states. */
export type NavIndicator = "live" | "pending" | "down" | "error";

// A single navigation entry. Exactly one leading mark: either an `icon`
// (a Svelte component rendered as `<Icon class="size-4" />`) or an
// `indicator` (a status dot). The discriminated union enforces mutual
// exclusivity at the type level; a third arm allows text-only entries.
export type NavItem =
  | {
      href: string;
      label: string;
      icon: Component;
      indicator?: never;
      badge?: number;
    }
  | {
      href: string;
      label: string;
      icon?: never;
      indicator: NavIndicator;
      badge?: number;
    }
  | {
      href: string;
      label: string;
      icon?: never;
      indicator?: never;
      badge?: number;
    };

// `badge` is the unread count for the entry (0 = absent; rendered through
// `badgeLabel` in NavLink, so the display caps at "99+"). Purely additive:
// the three leading-mark arms stay mutually exclusive.
