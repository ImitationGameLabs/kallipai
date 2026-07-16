<script lang="ts">
  import MonoBlock from "./MonoBlock.svelte";
  import StatChip from "./StatChip.svelte";

  // BgReadOutput (crates/kallip-shell/src/tools/bg_read.rs:29-43).
  interface BgReadOutput {
    output?: string;
    state?: string;
    exit_code?: number | null;
    bytes?: number;
    stalled?: boolean;
  }

  let { result }: { result: unknown } = $props();
  const out = $derived((result ?? {}) as BgReadOutput);

  const stateBadge = $derived(
    out.state === "running"
      ? "preset-filled-success-500"
      : out.state === "killed"
        ? "preset-filled-error-500"
        : "preset-tonal-surface",
  );
</script>

<div class="space-y-1.5">
  <div class="flex flex-wrap items-center gap-1.5 text-xs">
    {#if out.state}<span class="badge {stateBadge}">{out.state}</span>{/if}
    {#if out.exit_code !== undefined && out.exit_code !== null}
      <span class="badge preset-tonal-surface">exit {out.exit_code}</span>
    {/if}
    {#if out.stalled}<span class="badge preset-filled-warning-500">stalled</span
      >{/if}
  </div>
  {#if out.output}<MonoBlock text={out.output} />{/if}
  {#if out.bytes !== undefined}
    <div class="flex flex-wrap gap-1">
      <StatChip label="bytes" value={out.bytes} />
    </div>
  {/if}
</div>
