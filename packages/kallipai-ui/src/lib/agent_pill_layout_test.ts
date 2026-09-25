// Behavior tests for the pill-row layout helpers: the overflow arithmetic
// arithmetic is pure, so it is tested directly (the
// source-read pins in agent_pills_test.ts cover the wiring side).

import { assertEquals } from "@std/assert";
import { visibleCountWithinRows, visiblePills } from "./agentPillLayout.ts";

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

Deno.test("visibleCountWithinRows sums the first two visual rows", () => {
  // One row of three, then a row-height drop to two more.
  const tops = [0, 0, 0, 36, 36];
  assertEquals(visibleCountWithinRows(tops, 2), 5);
  // A third row exists: its pills fold into the overflow.
  assertEquals(visibleCountWithinRows([...tops, 72, 72], 2), 5);
});

Deno.test(
  "visibleCountWithinRows groups by relative drift, not exact top",
  () => {
    // Sub-pixel jitter inside one row must not split it into two rows;
    // a real row gap always opens a new one.
    assertEquals(visibleCountWithinRows([0, 0.4, -0.4, 0.2], 1), 4);
    assertEquals(visibleCountWithinRows([0, 0, 36, 36.3], 1), 2);
  },
);

Deno.test("visibleCountWithinRows survives empty and single-row inputs", () => {
  assertEquals(visibleCountWithinRows([], 2), 0);
  assertEquals(visibleCountWithinRows([0, 0, 0], 2), 3);
});
