// Why a click on a bubble was not the bubble's own: the reader was copying.
//
// A surface may let a whole bubble act as one target (a room row opens its
// discussion that way, and a control inside a bubble answers its own click).
// The rule that decides reads the reader's selection as the click found it,
// and the selection only counts when it started inside the box the surface
// handed in. Every surface reads the same shape off the DOM here, so the
// question is answered one way and unit-tested without a browser.

import type { SelectionAtClick } from "./bubble_click.ts";

/** The parts of a Selection the rule reads: where it starts, and whether it
 *  holds anything at all. */
export type SelectionShape = {
  readonly isCollapsed: boolean;
  readonly anchorNode: unknown;
} | null;

/** The box a selection has to start in: a bubble, or a control inside one. */
export type SelectionBox = { contains(node: unknown): boolean } | null;

/** The reader's selection as the click found it, read against one container.
 *  A selection with no anchor (a browser that lost it) counts as starting
 *  nowhere, which leaves the bubble still acting. */
export function selectionAtClickOf(
  selection: SelectionShape,
  container: SelectionBox,
): SelectionAtClick {
  if (selection === null) return null;
  return {
    collapsed: selection.isCollapsed,
    anchorInBox:
      container !== null &&
      selection.anchorNode !== null &&
      container.contains(selection.anchorNode),
  };
}

/** The same rule for a click event: the container is the element the listener
 *  was mounted on, which is the bubble body or the control itself. */
export function selectionAtClick(event: MouseEvent): SelectionAtClick {
  return selectionAtClickOf(
    globalThis.getSelection(),
    event.currentTarget instanceof Element ? event.currentTarget : null,
  );
}
