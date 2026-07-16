<script lang="ts">
  import type { TranscriptLine } from "@kallipai/kallip-common";
  import MonoBlock from "./MonoBlock.svelte";

  let { line }: { line: Extract<TranscriptLine, { kind: "toolCall" }> } =
    $props();

  // Pretty-print the args JSON; fall back to the raw string if it isn't JSON.
  const pretty = $derived.by(() => {
    try {
      return JSON.stringify(JSON.parse(line.args), null, 2);
    } catch {
      return line.args;
    }
  });
</script>

<details class="rounded-lg preset-tonal-surface">
  <summary
    class="cursor-pointer select-none px-2 py-1 font-mono text-xs opacity-70"
  >
    tool · {line.name}
  </summary>
  <div class="px-2 pb-2"><MonoBlock text={pretty} /></div>
</details>
