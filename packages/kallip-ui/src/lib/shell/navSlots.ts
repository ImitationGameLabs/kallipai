// Slot plan for the small-viewport bottom bar: cap the visible nav entries,
// route the rest (plus every section-manage gear) into the overflow sheet.
//
// A section with a `hub` never contributes items to the small screen at all:
// it folds into ONE cell (linking the hub page) BEFORE flat/hasMore are
// computed, so its items cannot overflow into the sheet either. Folding is
// therefore the first step below — computing flat from the raw sections
// would resurrect the More button this field exists to retire.
// The two app modes share one formula here WITHOUT the mode being passed in:
// a section `manage` gear exists only in online mode (see links.ts), and the
// gear's only small-screen home is the sheet — so its presence alone is the
// structural signal for "More is always shown". Offline (no gears) folds its
// manage section into a hub cell, so its two flat items never overflow —
// the formula stays a defensive general rule.
import type { NavItem } from "../shell.ts";
import type { NavSection } from "./links.ts";

export interface NavSlotPlan {
  /** Flat items shown as bar cells, in the same flat order the sheet slices.
   * May include a synthesized hub cell (from a section's `hub`) — it renders
   * like any item, and can never fall into the sheet. */
  visible: NavItem[];
  /** Flat items beyond the visible cap, in the same order. */
  overflow: NavItem[];
  /** Sections re-filtered to overflow items; a section stays when it still
   * contributes overflow items OR owns a manage gear (online's empty-section
   * case: the gear row is the only path to /tagmata for a fresh account). */
  sheetSections: NavSection[];
  /** Whether the bar renders a More button at all. */
  hasMore: boolean;
}

export function navSlots(links: NavSection[]): NavSlotPlan {
  const folded = links.map((s) => (s.hub ? { ...s, items: [] } : s));
  // The hub cell synthesizes into `flat` like any nav item, so the bar
  // renders it through the same loop; it can never reach the sheet
  // because `folded` holds no items for a hub section.
  const flat = links.flatMap((s) =>
    s.hub
      ? [{ href: s.hub.href, label: s.hub.label, icon: s.hub.icon }]
      : s.items
  );
  const hasManage = links.some((s) => s.manage);
  const hasMore = flat.length > 4 || hasManage;
  // NOTE: fold before measuring — see the header comment.
  // With More present the bar keeps at most 3 nav cells (More + Account fill
  // the remaining two of the 5-cell cap); without More every item fits.
  const visibleCount = hasMore ? Math.min(flat.length, 3) : flat.length;
  const visible = flat.slice(0, visibleCount);
  const overflow = flat.slice(visibleCount);
  const sheetSections = folded
    .map((s) => ({ ...s, items: s.items.filter((i) => overflow.includes(i)) }))
    .filter((s) => s.items.length > 0 || s.manage !== undefined);
  // The hub section is dropped outright here (folded has items: [] and
  // no manage gear), keeping its items out of the sheet for good.
  return { visible, overflow, sheetSections, hasMore };
}
