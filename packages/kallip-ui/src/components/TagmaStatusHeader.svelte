<script lang="ts">
  // Full-width chrome bar above the transcript (operator-directed redesign,
  // /tmp/plans/status-bar-design.md case B + rounds 2-3). One centered row
  // of two segments -- subagents / budget -- separated by a vertical
  // rule; justify-center clusters the content mid-bar on
  // ultra-wide screens instead of pinning the segments apart
  // across the void. The inner block centers at max-w-[56rem] --
  // tighter than the transcript column: the status-card area
  // clusters instead of stretching with the chat (operator ruling).
  // The agent rows below center at their own 40rem -- tighter still
  // (dense list); bar and rows widths are deliberately different.
  // The bar spans the full main area as chrome (the old centered
  // max-w-2xl card read as floating content). py-3 + min-h-14 give
  // the bar breathing height.
  //
  // Layering: surface-200-800 sits one shade above the sidebar's
  // 100-900 (the Navigation root paints that) — the same shade would
  // weld bar and sidebar into one region, and the original two-shade
  // drop read too heavy; one shade up is the operator's call. border-b
  // 400-600 keeps the seam legible in both modes (300-700 dips to ~4
  // ΔL oklab on dark); Skeleton's -contrast tokens are text-on-fill
  // tools, not this panel-on-page shade, so the pairing stays numeric.
  // The sidebar placement reuses 200-800 by the same chrome
  // precedent; its neighbour comparison differs from the bar's (a panel
  // on the page beside the transcript, not a bar beside the navigation),
  // so the shared tone is what ties the two forms to one area.
  //
  // Root state lives in the agent rows below -- its former bar segment
  // duplicated the root row (operator feedback). Pure/
  // presentational: takes the snapshot as a prop, so it is
  // transport-agnostic (the conversation owns the snapshot). Sits above
  // the transcript's scroll container, so it never scrolls away. Without a
  // snapshot (freshly connected, or an offline tagma) a slim waiting row
  // keeps the bar's height stable until the first tick (<= STATUS_INTERVAL,
  // ~2s).
  //
  // `sideLayout` renders the same status area as a right sidebar (aside)
  // beside the transcript instead of the top bar, gated to lg+ by the
  // page (matchMedia); a toggle in the area's corner swaps placements at
  // runtime, and the page persists the user's choice across reloads.
  //
  // Below the bar, inside the same header element, the agent-rows
  // section (TagmaAgentRows) extends the same tone: one line per agent,
  // root included (the bar keeps counts and budget only). It renders
  // only when the page supplies rows (a backend-attached conversation);
  // the bar stands alone otherwise.
  import {
    formatTokenCount,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import { ChevronDown, ChevronUp, PanelRight, PanelTop } from "@lucide/svelte";
  import {
    tagma_status_active_total,
    tagma_status_aria,
    tagma_status_budget,
    tagma_status_layout_toggle,
    tagma_status_subagents,
    tagma_status_waiting,
    tagma_status_show_details,
  } from "../paraglide/messages.js";
  import TagmaAgentRows from "./TagmaAgentRows.svelte";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";

  let {
    status,
    agentRows,
    sideLayout = false,
    onToggleSide,
  }: {
    status: TagmaStatusSummary | undefined;
    agentRows?: {
      rootRow: StatusCardRow | null;
      subRows: readonly StatusCardRow[];
    };
    /** Render as a right sidebar panel instead of the top bar (the page
     *  gates this to lg+). */
    sideLayout?: boolean;
    /** Flips the wanted placement (state owned and persisted by the page). */
    onToggleSide?: () => void;
  } = $props();

  // Small-viewport collapse (mobile B1): below lg the top bar's two segments
  // wrap and the agent-rows list renders unbounded -- together they eat half
  // a 375px viewport (operator-visible failure). The whole status area then
  // collapses to one summary line; tapping it restores the familiar bar +
  // rows for the session. The sidebar placement is lg+ by definition, so it
  // never collapses.
  const lgQuery = matchMedia("(min-width: 64rem)");
  let lgMatches = $state(lgQuery.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) =>
      (lgMatches = event.matches);
    lgQuery.addEventListener("change", onChange);
    return () => lgQuery.removeEventListener("change", onChange);
  });
  let expandedSmall = $state(false);
  const collapsedSmall = $derived(!sideLayout && !lgMatches && !expandedSmall);

  // Budget fill width, clamped to [0, 100]. 0 budget -> 0% (avoids div-by-zero).
  const budgetPct = $derived(
    status && status.tokenBudget > 0
      ? Math.min(100, (status.tokenConsumed / status.tokenBudget) * 100)
      : 0,
  );
</script>

{#snippet subagentsSegment(s: TagmaStatusSummary)}
  <!-- Segment 1: subagents, always shown (even 0/0). Shared verbatim by
       both placements. -->
  <div
    class="flex items-center gap-1.5"
    title={tagma_status_subagents() + " — " + tagma_status_active_total()}
  >
    <span class="text-base opacity-60">{tagma_status_subagents()}</span>
    <span
      class="size-2 rounded-full {s.subagentsActive > 0
        ? 'bg-success-500'
        : 'bg-surface-400-600'}"
      aria-hidden="true"
    ></span>
    <span class="font-medium">{s.subagentsActive}</span>
    <span>/</span>
    <span>{s.subagentsTotal}</span>
  </div>
{/snippet}

{#snippet budgetNumbers(s: TagmaStatusSummary)}
  <span class="tabular-nums whitespace-nowrap text-base">
    {formatTokenCount(s.tokenConsumed)} / {formatTokenCount(s.tokenBudget)}
  </span>
{/snippet}

{#snippet layoutToggle()}
  <!-- Layout toggle: swaps the status area between top bar and right
       sidebar (the page persists the choice). The icon shows the state
       a click would move TO, derived from the EFFECTIVE placement --
       below lg the sidebar never applies, so a dead click must not flip
       the icon -- and aria-pressed carries the current state. In the top
       bar it is absolutely positioned on the bar's right edge so the
       centred cluster does not shift; in the sidebar it sits in flow at
       the panel's top-right corner. -->
  <button
    type="button"
    onclick={onToggleSide}
    class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0 {sideLayout
      ? ''
      : 'absolute right-2 top-1/2 -translate-y-1/2'}"
    title={tagma_status_layout_toggle()}
    aria-label={tagma_status_layout_toggle()}
    aria-pressed={sideLayout}
  >
    {#if sideLayout}
      <PanelTop class="size-4" aria-hidden="true" />
    {:else}
      <PanelRight class="size-4" aria-hidden="true" />
    {/if}
  </button>
{/snippet}

<svelte:element
  this={sideLayout ? "aside" : "header"}
  class="bg-surface-200-800 border-surface-400-600 {sideLayout
    ? 'w-80 shrink-0 order-last flex flex-col border-l'
    : 'w-full border-b'}"
  aria-label={tagma_status_aria()}
>
  {#if sideLayout}
    <!-- Sidebar placement: the segments stack (a vertical rule between
         stacked blocks would read as a stray tick), budget label and
         numbers share one line with the track full-width below, and the
         rows section underneath becomes the panel's scroll region. The
         max-w-[56rem] centring of the top bar is dropped -- the panel is
         its own tight container. -->
    <div class="px-4 py-3 flex flex-col gap-2.5 text-lg">
      {#if status}
        <div class="flex items-center justify-between gap-2">
          {@render subagentsSegment(status)}
          {@render layoutToggle()}
        </div>
        <div class="flex flex-col gap-1.5">
          <div class="flex items-center gap-2">
            <span class="text-base opacity-60">{tagma_status_budget()}</span>
            <span class="flex-1"></span>
            {@render budgetNumbers(status)}
          </div>
          <div
            class="h-2 w-full rounded-full bg-surface-400-600 overflow-hidden"
          >
            <div
              class="h-full rounded-full bg-primary-500 transition-[width] duration-500"
              style="width: {budgetPct}%"
            ></div>
          </div>
        </div>
      {:else}
        <div class="flex items-center justify-between gap-2">
          <div class="flex items-center gap-1.5 text-base opacity-50">
            <span
              class="size-2 rounded-full bg-surface-400-600 animate-pulse"
              aria-hidden="true"
            ></span>
            <span>{tagma_status_waiting()}</span>
          </div>
          {@render layoutToggle()}
        </div>
      {/if}
    </div>
  {:else}
    {#if collapsedSmall}
      <!-- Collapsed summary line (see the script note): liveness dot +
           active/total + budget fill; the size-10 chevron meets T1 and
           restores the full bar + rows. -->
      <div
        class="mx-auto w-full max-w-[56rem] px-4 min-h-10 flex items-center gap-3 text-base"
      >
        {#if status}
          <span
            class="size-2 rounded-full shrink-0 {status.subagentsActive > 0
              ? 'bg-success-500'
              : 'bg-surface-400-600'}"
            aria-hidden="true"
          ></span>
          <span class="tabular-nums whitespace-nowrap"
            >{status.subagentsActive}/{status.subagentsTotal}</span
          >
          <span
            class="h-1.5 w-16 shrink-0 rounded-full bg-surface-400-600 overflow-hidden"
            aria-hidden="true"
          >
            <span
              class="block h-full rounded-full bg-primary-500"
              style="width: {budgetPct}%"
            ></span>
          </span>
        {:else}
          <span
            class="size-2 rounded-full bg-surface-400-600 animate-pulse"
            aria-hidden="true"
          ></span>
          <span class="opacity-50">{tagma_status_waiting()}</span>
        {/if}
        <span class="flex-1"></span>
        <button
          type="button"
          onclick={() => (expandedSmall = true)}
          class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0"
          aria-label={tagma_status_show_details()}
          aria-expanded="false"
        >
          <ChevronDown class="size-4" aria-hidden="true" />
        </button>
      </div>
    {:else}
      <!-- Top bar placement: one centred row of the two segments. The
         relative wrapper exists so the toggle can ride the bar's right
         edge (full width) rather than the 56rem cluster's edge. -->
      <div class="relative w-full">
        <div
          class="mx-auto w-full max-w-[56rem] px-4 py-3 flex flex-wrap items-center justify-center gap-x-4 gap-y-2 min-h-14 text-lg"
        >
          {#if status}
            {@render subagentsSegment(status)}
            <!-- Vertical rule between the two segments (aria-hidden: the
               title attributes carry the names to screen readers). -->
            <div
              class="w-px self-stretch bg-surface-400-600"
              aria-hidden="true"
            ></div>
            <!-- Segment 2: token budget. When the two segments no longer fit,
               flex-wrap starts a second centred row (justify-center on this
               container too, not a left pin). Track is 400-600 because it
               must clear the bar's tone in both modes. -->
            <div class="flex items-center gap-2">
              <span class="text-base opacity-60">{tagma_status_budget()}</span>
              <div
                class="h-2 w-56 shrink-0 rounded-full bg-surface-400-600 overflow-hidden"
              >
                <div
                  class="h-full rounded-full bg-primary-500 transition-[width] duration-500"
                  style="width: {budgetPct}%"
                ></div>
              </div>
              {@render budgetNumbers(status)}
            </div>
          {:else}
            <!-- No snapshot yet: keep the bar's height with a muted placeholder. -->
            <div class="flex items-center gap-1.5 text-base opacity-50">
              <span
                class="size-2 rounded-full bg-surface-400-600 animate-pulse"
                aria-hidden="true"
              ></span>
              <span>{tagma_status_waiting()}</span>
            </div>
          {/if}
        </div>
        {#if lgMatches}
          {@render layoutToggle()}
        {:else}
          <!-- Small expanded: the panel toggle is a dead click below lg, so
             this slot collapses back to the summary line instead. -->
          <button
            type="button"
            onclick={() => (expandedSmall = false)}
            class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0 absolute right-2 top-1/2 -translate-y-1/2"
            aria-label={tagma_status_show_details()}
            aria-expanded="true"
          >
            <ChevronUp class="size-4" aria-hidden="true" />
          </button>
        {/if}
      </div>
    {/if}
  {/if}
  {#if agentRows && !collapsedSmall}
    <TagmaAgentRows
      rootRow={agentRows.rootRow}
      subRows={agentRows.subRows}
      narrow={sideLayout}
    />
  {/if}
</svelte:element>
