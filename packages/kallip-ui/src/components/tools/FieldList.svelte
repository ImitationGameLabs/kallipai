<script lang="ts">
  // A definition list of "label · detail" rows.
  let {
    rows = [],
  }: {
    rows?: { label: string; detail?: string | number }[];
  } = $props();
</script>

<dl class="space-y-0.5 text-xs">
  {#each rows as row (row.label)}
    <div class="flex gap-2">
      <dt class="opacity-60">{row.label}</dt>
      <dd class="font-mono break-all">{row.detail ?? ""}</dd>
    </div>
  {/each}
</dl>
