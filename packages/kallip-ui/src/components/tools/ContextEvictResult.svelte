<script lang="ts">
  import StatChip from "./StatChip.svelte";

  // context_evict result (crates/kallip-runtime/src/tools/context/evict.rs:70-85):
  // counts only — the agent-written summary rides on the toolCall args.
  interface EvictOutput {
    evicted?: number;
    remaining_turns?: number;
    freed_tokens?: number;
  }

  let { result }: { result: unknown } = $props();
  const out = $derived((result ?? {}) as EvictOutput);
</script>

<div class="flex flex-wrap gap-1.5">
  <StatChip label="evicted" value={out.evicted ?? 0} />
  <StatChip label="remaining turns" value={out.remaining_turns ?? 0} />
  <StatChip label="freed tokens" value={out.freed_tokens ?? 0} />
</div>
