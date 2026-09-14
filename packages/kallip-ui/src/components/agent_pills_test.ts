// Source-read pins for the three new status surfaces (same rationale as
// scroll_to_bottom_button_test: the contracts are presentation wiring a
// typecheck cannot see). The pill row, the detail layer, and the drawer
// list must all draw their state marks from the shared tables, never a
// private mapping.

import { assert, assertEquals } from "@std/assert";

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
    // State marks come from the shared table (never a private mapping):
    // the badge nests the shared AgentStateIcon rendering (decorative
    // there), and the full state word rides tooltip+aria.
    assert(
      src.includes(
        '<AgentStateIcon state={row.state} size="size-3" label={false} />',
      ),
    );
    assert(src.includes('class="badge"'));
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
    // The mark is the shared AgentStateIcon rendering (which owns the
    // accessible name); the full state word rides the body line.
    assert(src.includes("<AgentStateIcon state={row.state} />"));
    assert(src.includes(">{agentStateLabel(row.state)}<"));
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
    // Root carries the badge; marks come from the shared rendering with
    // the full state word as the accessible name.
    assert(src.includes("tagma_status_root()"));
    assert(src.includes("<AgentStateIcon state={row.state} />"));
    // The summary line mirrors the status line aggregation.
    assert(src.includes('budget.rootState === "busy"'));
    assert(src.includes("tagma_status_waiting()"));
  },
);

const TABLE = new URL("../lib/agentState.ts", import.meta.url);

Deno.test(
  "the shared state table covers all six states in one encoding",
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
      assert(
        new RegExp("^  " + key + ": [{]", "m").test(src),
        `icon table must key ${key}`,
      );
    }
    // The retired char table must stay retired: one shape language,
    // one rendering site.
    assert(!src.includes("agentStateGlyph"));
    assert(!src.includes("GLYPHS"));
    // retrying draws the centered mark: the motion-reduce shape
    // distinction (solid ring / dashed ring / ring+mark).
    assert(src.includes('center: "!"'));
  },
);

const ICON = new URL("./AgentStateIcon.svelte", import.meta.url);
const ROWS = new URL("./TagmaAgentRows.svelte", import.meta.url);
const DOT = new URL("./manage/StateDot.svelte", import.meta.url);

Deno.test(
  "every state-bearing surface renders through AgentStateIcon",
  { permissions: { read: [ICON, PILLS, DETAIL, DRAWER, ROWS, DOT] } },
  () => {
    const icon = source(ICON);
    // The one rendering site: the shared comp, its classes, and the
    // still-centered overlay; the accessible name is optional so
    // decorative callers (the pill badge) can nest it.
    assert(icon.includes("agentStateIcon(state)"));
    assert(icon.includes("<spec.comp"));
    assert(icon.includes("{spec.center}"));
    assert(icon.includes("label = true"));
    for (const [name, src] of [
      ["pills", source(PILLS)],
      ["detail", source(DETAIL)],
      ["drawer", source(DRAWER)],
      ["rows", source(ROWS)],
      ["state dot", source(DOT)],
    ] as const) {
      assert(
        src.includes("<AgentStateIcon"),
        name + " must render through AgentStateIcon",
      );
    }
  },
);

Deno.test(
  "the overlay mark takes the tone pair, never the motion classes",
  { permissions: { read: [TABLE, ICON] } },
  () => {
    const table = source(TABLE);
    const icon = source(ICON);
    // Split dimensions: the tone rows hold only text-* pairs, the
    // motion rows only animate-* tokens. The overlay builds from the
    // tone pair alone -- if it inherited the motion dimension,
    // retrying's "!" would spin with the ring it is meant to stand
    // still against.
    const color = [...table.matchAll(/colorClassName: "([^"]*)"/g)].map(
      (m) => m[1],
    );
    assertEquals(color.length, 6, "every state declares its tone pair");
    for (const cls of color) {
      assert(!cls.includes("animate-"), "tone rows carry no motion");
    }
    const motion = [...table.matchAll(/motionClassName: "([^"]*)"/g)].map(
      (m) => m[1],
    );
    assert(motion.length >= 2, "the live states carry motion");
    for (const cls of motion) {
      assert(/^animate-[a-z0-9-]+$/.test(cls), "motion rows carry only motion");
    }
    // The overlay span lives in the {#if spec.center} block: it builds
    // from the tone pair alone and never names the motion dimension.
    const overlay = icon.slice(
      icon.indexOf("{#if spec.center}"),
      icon.indexOf("{/if}"),
    );
    assert(overlay.includes("spec.colorClassName"));
    assert(!overlay.includes("motionClassName"), "the overlay stays still");
  },
);
