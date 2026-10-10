<script lang="ts">
  // The shared fact table as the fleet panel shows it: one tagma-level block
  // for the whole tagma. The table is tagma-wide (the wire sends it once per
  // response, never once per agent), so it never rides an agent row -- the
  // per-agent position carries that agent's own parked reason instead. The
  // block scrolls on its own past a few rows (a long table must not push the
  // roster out of the panel) and renders nothing while the table is empty
  // (no facts means nothing to say). Presentational: the store owns the data.

  import type { FactView } from "@kallipai/kallipai-client";
  import { onMount } from "svelte";
  import { factLines } from "../lib/session/upstreamFacts.ts";
  import { tagma_facts_title } from "../paraglide/messages.js";

  let { facts }: { facts: readonly FactView[] } = $props();

  // Live "now" ticking once a minute, so the reopens countdowns stay fresh
  // without a re-fetch (EnrollmentCodeCard's pattern; minute granularity
  // matches formatRemaining).
  let now = $state(Date.now());
  onMount(() => {
    const id = setInterval(() => {
      now = Date.now();
    }, 60_000);
    return () => clearInterval(id);
  });
  const lines = $derived(factLines(facts, Math.floor(now / 1_000)));
</script>

{#if lines.length > 0}
  <section class="border-t border-surface-400-600 max-h-40 overflow-y-auto">
    <div class="px-4 py-2 text-base">
      <div class="flex items-center gap-2 py-1.5">
        <span class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {tagma_facts_title()}
        </span>
        <span class="flex-1"></span>
        <span class="tabular-nums text-sm opacity-80">{lines.length}</span>
      </div>
      {#each lines as line (line.key)}
        <div class="py-1">
          <div class="flex items-center gap-1.5 text-sm">
            <span class="font-medium">{line.kind}</span>
            <span class="opacity-60">{line.scope}</span>
            {#if line.endpoint}
              <span class="opacity-60 font-mono truncate">{line.endpoint}</span>
            {/if}
            <span class="opacity-60">{line.account}</span>
            {#if line.reopens}
              <span class="opacity-60">{line.reopens}</span>
            {/if}
          </div>
          <div class="truncate text-sm opacity-60" title={line.detail}>
            {line.detail}
          </div>
        </div>
      {/each}
    </div>
  </section>
{/if}
