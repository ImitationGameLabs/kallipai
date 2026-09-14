// Behavior tests for the pill-row layout helpers: the overflow arithmetic
// and the narrow label are pure, so they are tested directly (the
// source-read pins in agent_pills_test.ts cover the wiring side).

import { assertEquals } from "@std/assert";
import { pillLabel, visiblePills } from "./agentPillLayout.ts";

function row(id: string, role = id): { id: string; role: string } {
  return { id, role };
}

Deno.test("visiblePills cuts at the cap and counts the overflow", () => {
  const rows = [row("a"), row("b"), row("c")];
  assertEquals(visiblePills(rows, 5), {
    visible: rows,
    overflow: 0,
  });
  const cut = visiblePills(rows, 2);
  assertEquals(cut.visible, [rows[0], rows[1]]);
  assertEquals(cut.overflow, 1);
});

Deno.test("visiblePills survives an empty roster and a zero cap", () => {
  assertEquals(visiblePills([], 5), { visible: [], overflow: 0 });
  assertEquals(visiblePills([row("a")], 0), {
    visible: [],
    overflow: 1,
  });
});

Deno.test("pillLabel keeps names that fit and abbreviates the rest", () => {
  assertEquals(pillLabel(row("a", "reviewer-c"), 12), "reviewer-c");
  // Falls to the first grapheme when the name does not fit; ASCII gains
  // its uppercase form, CJK stays as-is (no case to change).
  assertEquals(pillLabel(row("a", "supercalifragilistic"), 8), "S");
  assertEquals(pillLabel(row("a", "首席评审员"), 4), "首");
  // The id backs the label when the role is empty.
  assertEquals(pillLabel(row("x7"), 1), "X");
});
