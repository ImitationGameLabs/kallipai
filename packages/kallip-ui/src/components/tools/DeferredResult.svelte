<script lang="ts">
  // The deferred (approval-pending) envelope: the tool call was committed but
  // awaits human approval before running.
  let { id, nextSteps }: { id: string; nextSteps: string } = $props();
</script>

<div class="space-y-1.5">
  <div class="flex items-center gap-1.5 text-xs">
    <span class="badge preset-filled-warning-500">pending approval</span>
    {#if id}<span class="font-mono opacity-60">{id}</span>{/if}
  </div>
  {#if nextSteps}<div class="text-xs opacity-70">{nextSteps}</div>{/if}
</div>
