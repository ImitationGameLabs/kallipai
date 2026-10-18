// Source-read pins for the top-bar status form (same rationale as
// scroll_to_bottom_button_test): the two-row pill row, the transient
// side panel behind the overflow chip, and the retired collapse
// machine are presentation contracts a typecheck cannot see.

import { assert } from "@std/assert";

const HEADER = new URL("./TagmaStatusHeader.svelte", import.meta.url);
const LAYOUT = new URL("../lib/shell/RootLayout.svelte", import.meta.url);
const ROWS = new URL("./TagmaAgentRows.svelte", import.meta.url);
const UPSTREAM = new URL("./UpstreamFacts.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the top bar always shows the pill row (no collapse machine)",
  { permissions: { read: [HEADER] } },
  () => {
    const src = source(HEADER);
    // The collapse state machine is retired: the wrapping pill row is
    // the one top-bar form, and the chevron pair rode only on it.
    assert(!src.includes("collapsedSmall"));
    assert(!src.includes("expandedSmall"));
    assert(!src.includes("ChevronDown"));
    assert(!src.includes("ChevronUp"));
    assert(!src.includes("tagma_status_show_details"));
    assert(!src.includes("AgentDrawerList"));
    // The overflow chip opens the transient panel; no roster dialog
    // state remains.
    assert(src.includes("let overflowOpen = $state(false);"));
    assert(src.includes("overflowOpen = true"));
    assert(!src.includes("rosterOpen"));
  },
);

Deno.test(
  "the transient panel mirrors the mobile drawer entrance",
  { permissions: { read: [HEADER, LAYOUT] } },
  () => {
    const header = source(HEADER);
    const layout = source(LAYOUT);
    // Right-anchored overlay: it covers part of the transcript without
    // squeezing it (portaled fixed grid), the mobile drawer anchor
    // mirrored to the right edge.
    assert(header.includes("justify-end"));
    assert(header.includes("[perspective:1200px]"));
    assert(header.includes("origin-left"));
    // Same entrance family as the mobile drawer: a perspective rotateY,
    // with reduced motion degraded to the plain slide.
    assert(layout.includes("rotateY(10deg)"));
    assert(header.includes("rotateY(10deg)"));
    assert(header.includes("motion-reduce:[transform:translateX(100%)]"));
    // The panel reuses the sidebar rows section (one list, one source
    // of marks) -- never the drawer list.
    assert(header.includes("<TagmaAgentRows"));
    assert(!header.includes("<AgentDrawerList"));
  },
);

Deno.test(
  "the budget numbers render the localized label when unlimited",
  { permissions: { read: [HEADER] } },
  () => {
    const src = source(HEADER);
    // The shared budgetNumbers snippet swaps the denominator for the
    // localized Unlimited label; the track's budgetPct degrades to 0
    // on its own (budget is 0 while unlimited), so no fill shows.
    assert(src.includes("s.tokenBudgetUnlimited"));
    assert(src.includes("? tagma_status_unlimited()"));
  },
);

Deno.test(
  "the shared fact table renders in the fleet panel, never per agent row",
  { permissions: { read: [HEADER, ROWS, UPSTREAM] } },
  () => {
    const header = source(HEADER);
    const rows = source(ROWS);
    const facts = source(UPSTREAM);
    // The table is tagma-wide: one block, mounted in both fleet containers
    // (the sidebar section and the transient panel -- the same two the rows
    // ride), always fed the tagma-level prop.
    const mounts = header.split("<UpstreamFacts {facts} />").length - 1;
    assert(
      mounts === 2,
      `expected the fact block in both fleet containers, found ${mounts}`,
    );
    assert(header.includes("facts?: readonly FactView[]"));
    // Per-agent rows carry that agent's own parked reason, never the table.
    assert(!rows.includes("UpstreamFacts"));
    assert(!rows.includes("FactView"));
    // An empty table renders no block at all; the lines come from the whole
    // table, not from one agent's slice of it.
    assert(facts.includes("{#if lines.length > 0}"));
    assert(facts.includes("factLines(facts,"));
  },
);

Deno.test(
  "the top bar renders the active-agents count segment",
  { permissions: { read: [HEADER] } },
  () => {
    const src = source(HEADER);
    // The count segment leads the pill row; pin its presence in the top bar.
    const mounts =
      src.split("{@render activeAgentsSegment(status)}").length - 1;
    assert(
      mounts === 3,
      `expected the count segment in all three fleet containers, found ${mounts}`,
    );
    const anchor = src.indexOf('<div class="relative w-full">');
    assert(anchor >= 0, "the top bar container is present");
    const bar = src.slice(anchor);
    assert(
      bar.includes("{@render activeAgentsSegment(status)}"),
      "the count segment is rendered in the top bar",
    );
    const segment = bar.indexOf("{@render activeAgentsSegment(status)}");
    const pills = bar.indexOf("<AgentPills");
    assert(pills > 0 && segment < pills, "the count segment leads the pills");
  },
);
