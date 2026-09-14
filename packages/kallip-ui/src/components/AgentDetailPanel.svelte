<script lang="ts">
  // The per-agent detail card the top-bar pills open: name, full
  // state (glyph + word, the shared tables), activity, context
  // occupancy, and the structured parked reason when present. Rendered
  // inside the popover content (anchored to the pill, dismissed by
  // Escape/outside tap), so this owns only the card. Purely
  // presentational.
  import { agentStateGlyph, agentStateLabel } from "../lib/agentState.ts";
  import { parkedReasonText } from "../lib/session/parkedReason.ts";
  import { contextOccupancy } from "../lib/tagmata.svelte.ts";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import {
    tagma_agent_detail_context,
    tagma_agent_detail_aria,
  } from "../paraglide/messages.js";

  let { row }: { row: StatusCardRow } = $props();

  const glyph = $derived(agentStateGlyph(row.state));
  // Same readout the drawer list renders: shared helper, one shape.
  const contextText = $derived(
    contextOccupancy(row.contextTokens, row.contextWindow),
  );
</script>

<div
  class="w-72 rounded-base bg-surface-50-950 p-3 text-base shadow-lg"
  aria-label={tagma_agent_detail_aria()}
>
  <div class="flex items-center gap-2 min-w-0">
    <span
      class="leading-none shrink-0 {glyph.className}"
      role="img"
      aria-label={agentStateLabel(row.state)}
      title={agentStateLabel(row.state)}>{glyph.char}</span
    >
    <span class="font-medium truncate">{row.role || row.id}</span>
  </div>
  <div class="text-sm opacity-80">{agentStateLabel(row.state)}</div>
  {#if row.activity}
    <div class="mt-1.5 text-sm truncate">{row.activity}</div>
  {/if}
  <div class="mt-1.5 flex items-center gap-2 text-sm">
    <span class="opacity-60">{tagma_agent_detail_context()}</span>
    <span class="tabular-nums whitespace-nowrap">{contextText}</span>
  </div>
  {#if row.description}
    <div class="mt-1.5 text-sm opacity-60">{row.description}</div>
  {/if}
  {#if row.parkedReason}
    <div class="mt-1.5 text-sm text-warning-600-400">
      {parkedReasonText(row.parkedReason)}
    </div>
  {/if}
</div>
