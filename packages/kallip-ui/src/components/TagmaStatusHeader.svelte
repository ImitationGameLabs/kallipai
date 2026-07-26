<script lang="ts">
  // A live status header for a running tagma, laid out as a two-column status
  // table so the eye scans two vertical columns: a muted right-aligned identity
  // label on the left (`root` / `subagents` / `budget`), and the state/value on
  // the right. The chrome is `card preset-tonal-surface` -- the project's card
  // idiom (same as TagmaCard). Agent chat bubbles are bare `preset-tonal-surface`
  // (no `card`), so the `card` utility's border + shadow is what separates the
  // header from the bubbles, without inventing an off-idiom background. Text
  // color is Skeleton's default (no hand-rolled opacity) so contrast stays within
  // the palette's guaranteed range.
  //
  // Root state is encoded with three redundant channels (icon shape, motion,
  // color) so it does not rely on color alone (colorblind-safe). Fed by the
  // `tagma_status` SSE event via realtimeStore, so it shares one source of truth
  // with the /tagmata dashboard cards (which render an aggregate). Sits above
  // the transcript in ChannelChatPage, width-matched to the chat column.
  //
  // When no snapshot has arrived yet (freshly connected, or an offline tagma)
  // the header stays mounted in a slim "waiting" state so layout does not shift
  // when the first tick lands (<= STATUS_INTERVAL, ~2s).
  import { realtimeStore } from "../lib/session/realtime.svelte";
  import {
    formatTokenCount,
    type TagmaAgentState,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import { Circle, LoaderCircle, TriangleAlert } from "@lucide/svelte";

  let { tagmaId }: { tagmaId: string } = $props();

  // Reactive snapshot read; `undefined` until the first `tagma_status` event.
  const status = $derived<TagmaStatusSummary | undefined>(
    realtimeStore.statusFor(tagmaId),
  );

  // Budget fill width, clamped to [0, 100]. 0 budget -> 0% (avoids div-by-zero).
  const budgetPct = $derived(
    status && status.tokenBudget > 0
      ? Math.min(100, (status.tokenConsumed / status.tokenBudget) * 100)
      : 0,
  );

  // Icon color by root state. The icon carries the color channel (shape +
  // motion + color); the state word stays plain text.
  function rootIconClass(state: TagmaAgentState): string {
    switch (state) {
      case "busy":
        return "text-success-500";
      case "faulted":
        return "text-error-500";
      case "idle":
        return "text-surface-500";
    }
  }
</script>

<header class="mx-auto w-full max-w-2xl px-4 pt-3" aria-label="Tagma status">
  <div class="card preset-tonal-surface px-4 py-3 flex flex-col gap-2 text-lg">
    {#if status}
      <!-- Row 1: root state. Icon shape + motion + color encode the state. -->
      <div class="flex items-center gap-3">
        <span class="w-24 shrink-0 text-right text-base">root</span>
        <div class="flex flex-1 items-center gap-1.5">
          {#if status.rootState === "busy"}
            <LoaderCircle
              class="size-5 animate-spin {rootIconClass(status.rootState)}"
              aria-hidden="true"
            />
          {:else if status.rootState === "faulted"}
            <TriangleAlert
              class="size-5 {rootIconClass(status.rootState)}"
              aria-hidden="true"
            />
          {:else}
            <Circle
              class="size-5 {rootIconClass(status.rootState)}"
              aria-hidden="true"
            />
          {/if}
          <span>{status.rootState}</span>
        </div>
      </div>
      <!-- Row 2: subagents. Always shown (even 0/0) so the header keeps a stable
           3-row height as subagents spawn/remove. -->
      <div class="flex items-center gap-3">
        <span class="w-24 shrink-0 text-right text-base">subagents</span>
        <div class="flex flex-1 items-center gap-1.5" title="active / total">
          <span
            class="size-2 rounded-full {status.subagentsActive > 0
              ? 'bg-success-500'
              : 'bg-surface-400'}"
            aria-hidden="true"
          ></span>
          <span class="font-medium">{status.subagentsActive}</span>
          <span>/</span>
          <span>{status.subagentsTotal}</span>
        </div>
      </div>
      <!-- Row 3: token budget. The bar gets the full row width to breathe. -->
      <div class="flex items-center gap-3">
        <span class="w-24 shrink-0 text-right text-base">budget</span>
        <div class="flex flex-1 items-center gap-2">
          <div class="h-2 flex-1 rounded-full bg-surface-200 overflow-hidden">
            <div
              class="h-full rounded-full bg-primary-500 transition-[width] duration-500"
              style="width: {budgetPct}%"
            ></div>
          </div>
          <span class="tabular-nums whitespace-nowrap text-base">
            {formatTokenCount(status.tokenConsumed)} / {formatTokenCount(
              status.tokenBudget,
            )}
          </span>
        </div>
      </div>
    {:else}
      <!-- No snapshot yet: keep the row's height with a muted placeholder. -->
      <div class="flex items-center gap-3 text-base opacity-50">
        <span class="w-24 shrink-0 text-right">status</span>
        <div class="flex flex-1 items-center gap-1.5">
          <span
            class="size-2 rounded-full bg-surface-400 animate-pulse"
            aria-hidden="true"
          ></span>
          <span>waiting…</span>
        </div>
      </div>
    {/if}
  </div>
</header>
