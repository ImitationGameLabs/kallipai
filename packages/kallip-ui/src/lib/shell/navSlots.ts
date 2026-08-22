// Slot plan for the small-viewport bottom bar: cap the visible nav entries,
// route the rest (plus every section-manage gear) into the overflow sheet.
//
// The two app modes share one formula here WITHOUT the mode being passed in:
// a section `manage` gear exists only in online mode (see links.ts), and the
// gear's only small-screen home is the sheet — so its presence alone is the
// structural signal for "More is always shown". Offline (no gears) shows More
// only when there is actual overflow, which for its structurally-fixed six
// items is always, but the formula stays a defensive general rule.
import type { NavItem } from "../shell.ts";
import type { NavSection } from "./links.ts";

export interface NavSlotPlan {
  /** Flat items shown as bar cells, in the same flat order the sheet slices. */
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
  const flat = links.flatMap((s) => s.items);
  const hasManage = links.some((s) => s.manage);
  const hasMore = flat.length > 4 || hasManage;
  // With More present the bar keeps at most 3 nav cells (More + Account fill
  // the remaining two of the 5-cell cap); without More every item fits.
  const visibleCount = hasMore ? Math.min(flat.length, 3) : flat.length;
  const visible = flat.slice(0, visibleCount);
  const overflow = flat.slice(visibleCount);
  const sheetSections = links
    .map((s) => ({ ...s, items: s.items.filter((i) => overflow.includes(i)) }))
    .filter((s) => s.items.length > 0 || s.manage !== undefined);
  return { visible, overflow, sheetSections, hasMore };
}
