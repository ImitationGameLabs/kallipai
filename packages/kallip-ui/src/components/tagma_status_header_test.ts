// Source-read pins for the top-bar status form (same rationale as
// scroll_to_bottom_button_test): the two-row pill row, the transient
// side panel behind the overflow chip, and the retired collapse
// machine are presentation contracts a typecheck cannot see.

import { assert } from "@std/assert";

const HEADER = new URL("./TagmaStatusHeader.svelte", import.meta.url);
const LAYOUT = new URL("../lib/shell/RootLayout.svelte", import.meta.url);

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
