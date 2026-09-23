<script lang="ts">
  // Recent-retries card for the agent detail page: relative/absolute toggle
  // (a self-contained pair of $state) and the show-all expansion. Line
  // formatting lives in lib/manage/retryFormat.ts; the relative clock is
  // read per render, so the 5s status poll refreshes the buckets on the
  // data's own cadence.

  import { CalendarClock, Clock } from "@lucide/svelte";
  import type { AgentStatusResponse } from "@kallipai/kallip-client";
  import {
    fmtAbsoluteRetry,
    fmtRelativeRetry,
  } from "../../lib/manage/retryFormat.ts";
  import { timezoneSetting } from "../../lib/time/stamp.svelte.ts";
  import {
    manage_agent_recent_retries,
    manage_agent_retry_show_absolute,
    manage_agent_retry_show_all,
    manage_agent_retry_show_less,
    manage_agent_retry_show_relative,
  } from "../../paraglide/messages.js";

  let { status }: { status: AgentStatusResponse } = $props();

  let retryMode = $state<"relative" | "absolute">("relative");
  let showAllRetries = $state(false);
</script>

<section class="card preset-tonal-surface p-5 space-y-2">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_agent_recent_retries()}
    </h2>
    <button
      type="button"
      aria-pressed={retryMode === "relative"}
      title={retryMode === "relative"
        ? manage_agent_retry_show_absolute()
        : manage_agent_retry_show_relative()}
      aria-label={retryMode === "relative"
        ? manage_agent_retry_show_absolute()
        : manage_agent_retry_show_relative()}
      onclick={() =>
        (retryMode = retryMode === "relative" ? "absolute" : "relative")}
      class="rounded p-1.5 text-surface-500 dark:text-surface-400 hover:bg-surface-200-800 transition"
    >
      {#if retryMode === "relative"}
        <CalendarClock class="size-4" />
      {:else}
        <Clock class="size-4" />
      {/if}
    </button>
  </div>
  {#each showAllRetries ? status.recent_retries : status.recent_retries.slice(0, 3) as r}
    <div class="text-xs opacity-70">
      {retryMode === "relative"
        ? fmtRelativeRetry(r, Math.floor(Date.now() / 1000))
        : fmtAbsoluteRetry(r, timezoneSetting.value)}
    </div>
  {/each}
  {#if status.recent_retries.length > 3}
    <button
      type="button"
      onclick={() => (showAllRetries = !showAllRetries)}
      class="text-xs opacity-60 hover:opacity-100 transition"
    >
      {showAllRetries
        ? manage_agent_retry_show_less()
        : manage_agent_retry_show_all({
            count: status.recent_retries.length,
          })}
    </button>
  {/if}
</section>
