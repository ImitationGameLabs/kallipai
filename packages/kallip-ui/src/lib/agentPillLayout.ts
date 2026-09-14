// Pure layout helpers for the agent pill row (AgentPills.svelte): the
// visible-cap slice with its overflow count.
// Pure so the overflow arithmetic is testable without a viewport (the
// missedLines precedent in transcript.svelte.ts).

export interface PillRow {
  readonly id: string;
  readonly role: string;
}

/** The pills that survive the visible cap, plus how many fold into "+N".
 * Root-first ordering is the caller's concern (the store already sorts);
 * this only cuts. */
export function visiblePills<T extends PillRow>(
  rows: readonly T[],
  cap: number,
): { visible: readonly T[]; overflow: number } {
  const visible = rows.slice(0, Math.max(0, cap));
  return { visible, overflow: rows.length - visible.length };
}
// How many pills fit in the first maxRows visual rows, given each
// pill's measured offset top (DOM order). A new row starts when the
// top drifts more than a few pixels from the current row's first
// pill (relative grouping: sub-pixel jitter at zoom levels can never
// split one row in two, a whole row-height gap always does). Hiding
// the tail only makes earlier rows looser, so this count is the
// safe cut: the survivors never overflow after the re-layout.
export function visibleCountWithinRows(
  tops: readonly number[],
  maxRows: number,
): number {
  const rowTolerancePx = 4;
  let visible = 0;
  let rowTop: number | null = null;
  let rowsSeen = 0;
  for (const top of tops) {
    if (rowTop === null || top - rowTop > rowTolerancePx) {
      rowsSeen++;
      rowTop = top;
      if (rowsSeen > maxRows) break;
    }
    visible++;
  }
  return visible;
}
