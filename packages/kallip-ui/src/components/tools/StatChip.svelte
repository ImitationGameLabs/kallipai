<script lang="ts">
  let { label, value }: { label: string; value: string | number } = $props();
</script>

<span class="preset-tonal-surface rounded-md px-2 py-0.5 text-xs">
  <span class="opacity-60">{label}</span>
  <span class="ml-1 font-medium">{value}</span>
</span>
