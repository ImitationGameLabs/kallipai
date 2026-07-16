// Types for the shared app shell. Kept in a plain `.ts` module (not inside the
// AppShell component) so consumers can import `NavItem` for type-only use.
import type { Component } from "svelte";

// A single navigation entry. `icon` is an optional Svelte component (the
// consumer owns the icon set, e.g. @lucide/svelte) that AppShell renders as
// `<Icon class="size-4" />`. Optional so text-only navigation still works.
export type NavItem = {
  href: string;
  label: string;
  icon?: Component;
};
