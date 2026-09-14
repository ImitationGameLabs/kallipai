// Pure layout helpers for the agent pill row (AgentPills.svelte): the
// visible-cap slice with its overflow count, and the compact pill label.
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

/** Compact pill label for the narrow variant: the name when it fits, else
 * the first grapheme (codepoint-safe for CJK; uppercased only when the
 * letter has case). */
export function pillLabel(row: PillRow, max: number): string {
  const name = row.role || row.id;
  if (name.length <= max) return name;
  const first = Array.from(name)[0] ?? name;
  return first.toUpperCase() === first ? first : first.toUpperCase();
}
