// Source-read pins for the three new status surfaces (same rationale as
// scroll_to_bottom_button_test: the contracts are presentation wiring a
// typecheck cannot see). The pill row, the detail layer, and the drawer
// list must all draw their state marks from the shared tables, never a
// private mapping.

import { assert } from "@std/assert";

const PILLS = new URL("./AgentPills.svelte", import.meta.url);
const DETAIL = new URL("./AgentDetailPanel.svelte", import.meta.url);
const DRAWER = new URL("./AgentDrawerList.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the pill row draws state from the shared table and folds the overflow",
  { permissions: { read: [PILLS] } },
  () => {
    const src = source(PILLS);
    // State marks come from the shared glyph table (never a private
    // mapping): the badge container renders the glyph char (shape and
    // color), and the full state word rides tooltip and aria.
    assert(src.includes("agentStateGlyph(row.state)"));
    assert(src.includes('class="badge {glyph.className}"'));
    assert(src.includes("agentStateLabel(row.state)"));
    // Chips use the Skeleton chip utility with the filter-chip
    // selected pattern; the detail popover anchors to the chip.
    assert(src.includes("chip "));
    assert(src.includes("preset-filled-primary-200-800"));
    assert(
      src.includes(
        "preset-outlined-surface-500 hover:preset-filled-surface-500",
      ),
    );
    assert(src.includes("<Popover"));
    assert(src.includes("<Popover.Anchor>"));
    // The overflow pill carries the count and hands the click back.
    assert(src.includes("pills.overflow > 0"));
    assert(src.includes("tagma_status_more({ count: pills.overflow })"));
    assert(src.includes("onOverflow?.()"));
    // The budget indicator pins right: native progress + tabular
    // numbers from the live aggregate.
    assert(src.includes("ms-auto"));
    assert(src.includes('class="progress h-1.5'));
    assert(src.includes("formatTokenCount(budget.tokenConsumed)"));
  },
);

Deno.test(
  "the detail layer announces the full state word and the parked reason",
  { permissions: { read: [DETAIL] } },
  () => {
    const src = source(DETAIL);
    // The full state word rides both the glyph name and the body line.
    assert(
      (src.match(/agentStateLabel\(row\.state\)/g) ?? []).length >= 3,
      "the state word must label the glyph and the body",
    );
    // Context occupancy and the structured parked reason render.
    assert(src.includes("tagma_agent_detail_context()"));
    assert(src.includes("parkedReasonText(row.parkedReason)"));
    // The panel root carries the detail aria-label (the i18n key the
    // shell announces when a pill opens the layer).
    assert(src.includes("tagma_agent_detail_aria()"));
    // Positioning and dismissal belong to the Popover shell: the card
    // is pure content (no close button, no absolute anchor).
  },
);

Deno.test(
  "the drawer list renders every row unfolded with root anchored",
  { permissions: { read: [DRAWER] } },
  () => {
    const src = source(DRAWER);
    // Root first, then the store's attention order — no fold anywhere:
    // the list region scrolls, it never collapses rows.
    assert(src.includes("rootRow ? [rootRow, ...subRows] : [...subRows]"));
    assert(!src.includes("VISIBLE_SUBS"), "no fold constant may exist");
    assert(src.includes("overflow-y-auto"));
    assert(!src.includes("tagma_status_more("), "no +N pill in the list");
    // Root carries the badge; icons come from the shared table with the
    // full state word as the accessible name.
    assert(src.includes("tagma_status_root()"));
    assert(src.includes("agentStateIcon(row.state)"));
    assert(src.includes("aria-label={agentStateLabel(row.state)}"));
    // The summary line mirrors the status line aggregation.
    assert(src.includes('budget.rootState === "busy"'));
    assert(src.includes("tagma_status_waiting()"));
  },
);

const TABLE = new URL("../lib/agentState.ts", import.meta.url);

Deno.test(
  "the shared glyph table covers all six states with both encodings",
  { permissions: { read: [TABLE] } },
  () => {
    const src = source(TABLE);
    for (const key of [
      "idle",
      "busy",
      "waiting",
      "retrying",
      "parked",
      "faulted",
    ]) {
      assert(src.includes(`  ${key}: { char:`), `glyph table must key ${key}`);
      assert(
        new RegExp("^  " + key + ": [{]", "m").test(src),
        `icon table must key ${key}`,
      );
    }
  },
);
