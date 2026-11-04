// Which argument fields a call is worth summarising by, per tool name. A
// small hand-kept table on purpose: the alternative -- guessing from the
// first long string -- reads well in a demo and picks the wrong field in a
// record nobody looked at.
//
// Two rules:
//
// - A name enters the table only when its arguments really carry a field
//   worth reading first, and the field name is the one the tool's arguments
//   use on the wire. A test pins both (the table's keys against the tool
//   names this tagma actually records, and each field against a real
//   argument fixture), so a typo cannot fail silently into "no summary".
// - A missing field means no summary line at all, never an empty one: the
//   call still renders, one line shorter.

import { type JsonNode, plainScalar } from "./jsonView.ts";

/** Tool name -> the argument fields to show above the call's body, in the
 * order they are read. */
export const SUMMARY_FIELDS: Record<string, readonly string[]> = {
  bash_exec: ["timeout"],
  // "command" is deliberately not here: the terminal block already draws the
  // command line, from the one place that knows that field (terminalCommand).
};

export type SummaryField = { field: string; value: string };

/** The fields this build would show for a tool, before looking at a record. */
export function summaryFieldsFor(toolName: string): readonly string[] {
  return SUMMARY_FIELDS[toolName] ?? [];
}

/**
 * The summary a call gets: every table field that is present in the parsed
 * arguments and holds a scalar. Values are returned bare (no quotes, no
 * folding) -- the line that shows them owns the cut, so one long command
 * never decides here how it should be drawn.
 */
export function summarize(
  toolName: string,
  args: JsonNode | null,
): SummaryField[] {
  const fields = summaryFieldsFor(toolName);
  if (fields.length === 0 || args === null) return [];
  if (args.kind !== "object") return [];
  const found: SummaryField[] = [];
  for (const field of fields) {
    for (const entry of args.entries) {
      if (entry.key !== field) continue;
      const value = plainScalar(entry.node);
      if (value !== null) found.push({ field, value });
      break;
    }
  }
  return found;
}
