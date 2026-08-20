<script lang="ts">
  // Agent rows under the status bar: the roster the bar only summarizes
  // (subagent counts, budget) -- root's state lives here, not in the bar,
  // its per-state Lucide icons keeping shape + color (colorblind-safe).
  // Root first (the conversation owner anchors the roster), then subs in
  // attention order with idle folding away beyond five visible rows. The
  // fold row doubles as the toggle; expanded, the container scrolls with
  // root sticky so the root agent stays in view. Purely presentational:
  // the store (and its debounced row identity) lives in the page.
  //
  // The `narrow` variant renders inside the sidebar placement
  // (TagmaStatusHeader side layout): rows go two-line so live activity
  // survives the width, and the section itself becomes the panel's scroll
  // region.

  import { parkedReasonText } from "../lib/session/parkedReason.ts";
  import { type StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import {
    agentStateIcon,
    agentStateLabel,
    type AgentLifecycleState,
  } from "../lib/agentState.ts";
  import { formatTokenCount } from "../lib/tagmata.svelte.ts";
  import {
    tagma_status_collapse,
    tagma_status_idle_hidden,
    tagma_status_more,
    tagma_status_root,
  } from "../paraglide/messages.js";

  let {
    rootRow,
    subRows,
    narrow = false,
  }: {
    rootRow: StatusCardRow | null;
    subRows: readonly StatusCardRow[];
    /** Sidebar variant: the status panel is a narrow right column. */
    narrow?: boolean;
  } = $props();

  const VISIBLE_SUBS = 5;

  let expanded = $state(false);

  const visibleSubs = $derived(
    expanded ? subRows : subRows.slice(0, VISIBLE_SUBS),
  );
  const hiddenCount = $derived(subRows.length - visibleSubs.length);
  // The fold row reads differently when everything hidden is idle:
  // "nothing interesting down there" instead of a count of unknowns.
  const hiddenAllIdle = $derived(
    subRows.slice(VISIBLE_SUBS).every((r) => r.state === "idle"),
  );

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
  <!-- The wrapper span owns the tooltip and the a11y name: @lucide/svelte
       does not forward its title prop to the DOM (hand-tested), so the
       native tooltip must live on a plain element. -->
  {@const spec = agentStateIcon(state)}
  <span
    class="shrink-0 leading-none"
    title={agentStateLabel(state)}
    role="img"
    aria-label={agentStateLabel(state)}
  >
    <spec.comp class="size-5 {spec.className}" aria-hidden="true" />
  </span>
{/snippet}

{#if rootRow || subRows.length > 0}
  <!-- the bar/rows seam: same 400-600 as the header's border (border-b top
       bar / border-l sidebar). In the narrow variant this section is also
       the panel's scroll region: flex-1 min-h-0 takes the leftover panel
       height and actually scrolls (nested-flex min-content trap). The
       max-w centring below is a top-bar-only idea -- the sidebar is
       its own tight container (see TagmaStatusHeader). The top-bar row
       block centers at 40rem, deliberately narrower than the bar's
       56rem (a dense list reads better tight; operator call) -- the two
       widths are each their own, not a drift. -->
  <div
    class="border-t border-surface-400-600 {narrow
      ? 'flex-1 min-h-0 overflow-y-auto'
      : ''}"
  >
    <div
      class="{narrow
        ? 'px-4'
        : 'mx-auto w-full max-w-[40rem] px-4'} text-base {expanded && !narrow
        ? 'max-h-[50vh] overflow-y-auto'
        : ''}"
    >
      {#if rootRow}
        <!-- sticky bg = header bar tone (200-800); keeps scrolled rows
             hidden beneath. Unconditional in the narrow variant:
             there the section itself is the scroll region, so root must
             ride it; the top-bar variant scrolls only when expanded. -->
        <div
          class="flex items-center gap-2 py-1.5 {expanded || narrow
            ? 'sticky top-0 bg-surface-200-800'
            : ''}"
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
      {#each visibleSubs as row (row.id)}
        {#if narrow}
          <!-- Sidebar rows go two-line: name + context up front, live
               activity below (roster convention); a quiet row stays
               single-line. -->
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
        {:else}
          <div class="flex items-center gap-2 py-1.5" title={rowTooltip(row)}>
            {@render stateIcon(row.state)}
            <span class="font-medium truncate">{row.role || row.id}</span>
            {#if row.activity}
              <span class="truncate opacity-60">{row.activity}</span>
            {/if}
            <span class="flex-1"></span>
            <span class="tabular-nums whitespace-nowrap text-sm opacity-80">
              {contextText(row)}
            </span>
          </div>
        {/if}
      {/each}
      {#if hiddenCount > 0 || expanded}
        <button
          type="button"
          class="flex w-full items-center gap-2 py-1.5 cursor-pointer opacity-70 hover:opacity-100"
          onclick={() => (expanded = !expanded)}
        >
          <span class="text-sm"
            >{expanded
              ? tagma_status_collapse()
              : hiddenAllIdle
                ? tagma_status_idle_hidden({ count: hiddenCount })
                : tagma_status_more({ count: hiddenCount })}</span
          >
        </button>
      {/if}
    </div>
  </div>
{/if}
