// Types for the shared app shell. Kept in a plain `.ts` module (not inside the
// AppShell component) so consumers can import `NavItem` for type-only use.
import type { Component } from "svelte";

/** A small status indicator AppShell renders as a leading dot instead of an
 * icon (e.g. per-chat liveness in the sidebar). AppShell owns the visual
 * tokens; consumers map their domain state to this tri-state (+ error). */
export type NavIndicator = "live" | "pending" | "down" | "error";

// A single navigation entry. Exactly one leading mark: either an `icon`
// (a Svelte component rendered as `<Icon class="size-4" />`) or an
// `indicator` (a status dot). The discriminated union enforces mutual
// exclusivity at the type level; a third arm allows text-only entries.
export type NavItem =
  | { href: string; label: string; icon: Component; indicator?: never }
  | { href: string; label: string; icon?: never; indicator: NavIndicator }
  | { href: string; label: string; icon?: never; indicator?: never };
