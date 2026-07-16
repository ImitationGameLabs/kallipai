<script lang="ts">
  import StatChip from "./StatChip.svelte";
  import FieldList from "./FieldList.svelte";

  // context_status result (crates/kallip-runtime/src/tools/context/status.rs:46-59).
  interface StatusUsage {
    pinned_tokens?: number;
    turn_tokens?: number;
  }
  interface CumulativeUsage {
    consumed?: number;
  }
  interface StatusOutput {
    last_prompt_tokens?: number;
    usage?: StatusUsage;
    pinned_items?: [string, number][];
    turn_count?: number;
    cumulative_usage?: CumulativeUsage;
  }

  let { result }: { result: unknown } = $props();
  const out = $derived((result ?? {}) as StatusOutput);
  const pinned = $derived(
    (out.pinned_items ?? []).map(([label, tokens]) => ({
      label,
      detail: tokens,
    })),
  );
</script>

<div class="space-y-2">
  <div class="flex flex-wrap gap-1.5">
    {#if out.last_prompt_tokens !== undefined}
      <StatChip label="last prompt" value={out.last_prompt_tokens} />
    {/if}
    {#if out.usage?.pinned_tokens !== undefined}
      <StatChip label="pinned" value={out.usage.pinned_tokens} />
    {/if}
    {#if out.usage?.turn_tokens !== undefined}
      <StatChip label="turn" value={out.usage.turn_tokens} />
    {/if}
    {#if out.turn_count !== undefined}<StatChip
        label="turns"
        value={out.turn_count}
      />{/if}
    {#if out.cumulative_usage?.consumed !== undefined}
      <StatChip label="consumed" value={out.cumulative_usage.consumed} />
    {/if}
  </div>
  {#if pinned.length > 0}
    <div>
      <div class="text-xs opacity-50 mb-1">pinned items</div>
      <FieldList rows={pinned} />
    </div>
  {/if}
</div>
