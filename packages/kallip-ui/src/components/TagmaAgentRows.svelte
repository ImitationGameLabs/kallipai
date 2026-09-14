<script lang="ts">
  // Agent rows inside the sidebar placement (TagmaStatusHeader side
  // layout): the roster the summary only counts -- root's state lives
  // here, its per-state Lucide icons keeping shape + color
  // (colorblind-safe). Root first (the conversation owner anchors the
  // roster), then subs in attention order. Every row stays visible: the
  // section is the panel's scroll region (no fold in a column
  // this tall). Purely presentational: the store (and its debounced row
  // identity) lives in the page.

  import { parkedReasonText } from "../lib/session/parkedReason.ts";
  import { type StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import { type AgentLifecycleState } from "../lib/agentState.ts";
  import AgentStateIcon from "./AgentStateIcon.svelte";
  import { formatTokenCount } from "../lib/tagmata.svelte.ts";
  import { tagma_status_root } from "../paraglide/messages.js";

  let {
    rootRow,
    subRows,
  }: {
    rootRow: StatusCardRow | null;
    subRows: readonly StatusCardRow[];
  } = $props();

  // Row hover tooltip: the wire's description first (the human words), the
  // parked reason appended when present (why it stopped still matters);
  // neither -> undefined, which Svelte turns into no title attribute at
  // all (a uuid is not a tooltip).
  function rowTooltip(row: StatusCardRow): string | undefined {
    const parts = [
      row.description,
      row.parkedReason ? parkedReasonText(row.parkedReason) : undefined,
    ].filter((p): p is string => !!p);
    return parts.length > 0 ? parts.join(" — ") : undefined;
  }

  // Right-edge context column: "used / window" once the slow poll lands
  // ("—" for the window while the registry pull has not landed or the
  // profile is unknown); a bare "—" when even the used half is unknown.
  function contextText(row: StatusCardRow): string {
    if (row.contextTokens === null) return "—";
    const used = formatTokenCount(row.contextTokens);
    return row.contextWindow === null
      ? `${used} / —`
      : `${used} / ${formatTokenCount(row.contextWindow)}`;
  }
</script>

{#snippet stateIcon(state: AgentLifecycleState)}
  <!-- The shared rendering owns the tooltip and the a11y name. -->
  <AgentStateIcon {state} />
{/snippet}

{#if rootRow || subRows.length > 0}
  <!-- the bar/rows seam: same 400-600 as the header's border (border-l
       sidebar). This section is also the panel's scroll region:
       flex-1 min-h-0 takes the leftover panel height and actually
       scrolls (nested-flex min-content trap). Rows go two-line so live
       activity survives the width; a quiet row stays single-line. -->
  <div class="border-t border-surface-400-600 flex-1 min-h-0 overflow-y-auto">
    <div class="px-4 text-base">
      {#if rootRow}
        <!-- sticky bg = header bar tone (200-800); keeps scrolled rows
             hidden beneath. Unconditional: the section itself is the
             scroll region, so root must ride it. -->
        <div
          class="flex items-center gap-2 py-1.5 sticky top-0 bg-surface-200-800"
          title={rowTooltip(rootRow) ?? tagma_status_root()}
        >
          {@render stateIcon(rootRow.state)}
          <span class="font-medium">{tagma_status_root()}</span>
          <span class="flex-1"></span>
          <span class="tabular-nums whitespace-nowrap text-sm opacity-80">
            {contextText(rootRow)}
          </span>
        </div>
      {/if}
      {#each subRows as row (row.id)}
        <div class="py-1.5" title={rowTooltip(row)}>
          <div class="flex items-center gap-2">
            {@render stateIcon(row.state)}
            <span class="font-medium truncate">{row.role || row.id}</span>
            <span class="flex-1"></span>
            <span class="tabular-nums whitespace-nowrap text-sm opacity-80">
              {contextText(row)}
            </span>
          </div>
          {#if row.activity}
            <div class="truncate text-sm opacity-60 ps-7">{row.activity}</div>
          {/if}
        </div>
      {/each}
    </div>
  </div>
{/if}
