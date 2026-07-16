<script lang="ts">
  // A flexing row of tonal badge chips, one per label.
  let {
    items = [],
  }: {
    items?: { label: string }[];
  } = $props();
</script>

<div class="flex flex-wrap gap-1">
  {#each items as item (item.label)}
    <span class="badge preset-tonal-surface">{item.label}</span>
  {/each}
</div>
