<script lang="ts">
  import MonoBlock from "./MonoBlock.svelte";

  // Fallback: pretty-printed JSON. Used for the flat approval_* responses,
  // unknown tools, and any payload we do not render specially.
  let { data }: { data: unknown } = $props();

  const pretty = $derived.by(() => {
    try {
      return JSON.stringify(data, null, 2);
    } catch {
      return String(data);
    }
  });
</script>

<MonoBlock text={pretty} />
