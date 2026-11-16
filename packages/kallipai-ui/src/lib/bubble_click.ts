// Whether a click on a bubble body is the bubble's own, or someone else's.
//
// A surface may let the whole bubble act as one target (the room row opens its
// discussion that way). Two things inside a bubble are not part of that target:
// an inner control -- the quote block, the file download, a link in the
// rendered markdown -- handles its own click, and a pointer drag that selected
// text was the reader copying half a sentence, not asking to navigate.
// The search for an inner control stops at the bubble's edge: a control
// above the bubble (a row that is a button, a panel header) is not one of
// the bubble's own.
//
// Pure: the caller passes what it read off the DOM (the click target, the box,
// where the selection sat) so the decision is unit-tested without a browser.

/** Elements inside a bubble that answer clicks themselves. */
export const INNER_CONTROL =
  "button, a, input, textarea, select, [role='button'], [role='link']";

/** What the click landed on: an element carries `closest`, and a text node, a
 *  pseudo-element target, or a synthetic event carries none. */
export type ClickTarget = { closest(selector: string): unknown } | null;

/** The reader's selection at the moment of the click, as the surface read it:
 *  collapsed (a caret) or not, and anchored inside the container the surface
 *  handed in -- a bubble, or a control inside one (the summary bar asks the
 *  same question of itself).
 *
 *  Only where the selection STARTS is asked. A drag that begins outside the
 *  bubble and ends inside it selected text the reader found elsewhere, so it
 *  is not the bubble's copy and the bubble still acts. */
export type SelectionAtClick = {
  readonly collapsed: boolean;
  readonly anchorInBox: boolean;
} | null;

/** The bubble the surface decided for: the search for an inner control
 *  stops at its edge, so a control above it is not mistaken for one of the
 *  bubble's own. */
export type BubbleBox = { contains(node: unknown): boolean } | null | undefined;

/** Whether the reader's selection holds text started inside the bubble:
 *  the rule a control inside the bubble obeys too, since the body's gate
 *  never sees a click that landed on the control itself. */
export function selectionHoldsText(selection: SelectionAtClick): boolean {
  return selection !== null && !selection.collapsed && selection.anchorInBox;
}

/** Whether the bubble body should act on this click. */
export function bubbleActivates(
  target: ClickTarget,
  selection: SelectionAtClick,
  box: BubbleBox,
): boolean {
  const control = target?.closest(INNER_CONTROL) ?? null;
  if (control !== null && box?.contains(control) === true) return false;
  return !selectionHoldsText(selection);
}
