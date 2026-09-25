// Source-read pins for the status surfaces (same rationale as
// scroll_to_bottom_button_test: the contracts are presentation wiring a
// typecheck cannot see). The pill row and the drawer list must both
// draw their state marks from the shared tables, never a private mapping.

import { assert, assertEquals } from "@std/assert";

const PILLS = new URL("./AgentPills.svelte", import.meta.url);
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
    // there), and the full state word rides the tooltip.
    assert(
      src.includes(
        '<AgentStateIcon state={row.state} size="size-3" label={false} />',
      ),
    );
    assert(src.includes('class="badge"'));
    assert(src.includes("agentStateLabel(row.state)"));
    // Pills are non-interactive labels: the tooltip carries the full
    // name plus the state word; no popover and no aria-label remain.
    assert(!src.includes('aria-label="{row.role'), "no pill aria-label");
    assert(src.includes("chip "));
    assert(src.includes("preset-outlined-surface-500"));
    assert(!src.includes("<Popover"));
    assert(!src.includes("openId"));
    assert(!src.includes("aria-expanded"), "pills carry no open state");
    assert(
      src.includes(
        'title="{row.role || row.id} - {agentStateLabel(row.state)}"',
      ),
    );
    // The overflow pill carries the count and hands the click back.
    assert(src.includes("pills.overflow > 0"));
    assert(src.includes("tagma_status_more({ count: pills.overflow })"));
    assert(src.includes("onOverflow?.()"));
    // The row wraps to at most two measured rows: the cap is read off
    // a hidden mirror that always renders the full roster, so the
    // visible row never flashes an uncapped layout mid-measure. The
    // budget indicator pins right out of the wrap flow: native
    // progress + tabular numbers from the live aggregate.
    assert(src.includes("flex flex-wrap"));
    assert(src.includes("bind:this={mirror}"));
    assert(src.includes("invisible pointer-events-none"));
    assert(
      !src.includes("cap = Number.POSITIVE_INFINITY;"),
      "no uncapped reset flash",
    );
    assert(src.includes("visibleCountWithinRows"));
    // One style string, exactly two render sites: the visible row
    // and the hidden mirror must stay in lockstep, so a one-sided
    // class drift trips this count instead of silently diverging.
    assertEquals(
      src.split("chip preset-outlined-surface-500 max-w-[10rem]").length - 1,
      2,
    );
    assert(src.includes('class="flex items-center gap-1.5 shrink-0"'));
    assert(src.includes('class="progress h-1.5'));
    assert(src.includes("formatTokenCount(budget.tokenConsumed)"));
    // Unlimited budget: the native progress is gated off (the BudgetBar
    // unlimited precedent) and the denominator is the localized label.
    assert(src.includes("{#if !budget.tokenBudgetUnlimited}"));
    assert(src.includes("? tagma_status_unlimited()"));
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
    // Unlimited budget: the fill track is gated off and the denominator
    // renders the localized label (BudgetBar unlimited precedent).
    assert(src.includes("{#if !budget.tokenBudgetUnlimited}"));
    assert(src.includes("? tagma_status_unlimited()"));
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
  { permissions: { read: [ICON, PILLS, DRAWER, ROWS, DOT] } },
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
