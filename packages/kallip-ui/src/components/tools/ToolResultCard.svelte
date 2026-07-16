<script lang="ts">
  import type { Component } from "svelte";
  import type { TranscriptLine } from "@kallipai/kallip-common";
  import { parseToolResult } from "../../lib/tools/parse";
  import { shapeFields } from "../../lib/tools/shape";
  import BashExecResult from "./BashExecResult.svelte";
  import BgReadResult from "./BgReadResult.svelte";
  import ContextEvictResult from "./ContextEvictResult.svelte";
  import ContextStatusResult from "./ContextStatusResult.svelte";
  import DeferredResult from "./DeferredResult.svelte";
  import GenericResult from "./GenericResult.svelte";
  import FieldList from "./FieldList.svelte";
  import ChipRow from "./ChipRow.svelte";

  let { line }: { line: Extract<TranscriptLine, { kind: "toolResult" }> } =
    $props();

  // name -> component for success payloads with a dedicated renderer.
  // approval_redeem carries the INNER tool's name, so a redeemed bash_exec
  // dispatches to BashExecResult with no special-casing.
  const REGISTRY: Record<string, Component<{ result: unknown }>> = {
    bash_exec: BashExecResult,
    bash_background_read: BgReadResult,
    context_evict: ContextEvictResult,
    context_status: ContextStatusResult,
  };

  const parsed = $derived(parseToolResult(line.result));

  // Tools whose flat fields are shaped by shapeFields() into FieldList rows.
  const shaped = $derived(
    parsed.kind === "success"
      ? shapeFields(parsed.toolName, parsed.result)
      : null,
  );
</script>

<details class="rounded-lg preset-tonal-surface">
  <summary
    class="cursor-pointer select-none px-2 py-1 font-mono text-xs opacity-70"
  >
    {#if parsed.kind === "success" || parsed.kind === "deferred" || parsed.kind === "error"}
      result · {parsed.toolName}
    {:else}
      result
    {/if}
  </summary>

  <div class="px-2 pb-2 space-y-1.5">
    {#if parsed.kind === "error"}
      <div class="text-xs text-error-500">
        [error] {parsed.toolName}: {parsed.error}
      </div>
    {:else if parsed.kind === "deferred"}
      <DeferredResult id={parsed.id} nextSteps={parsed.nextSteps} />
    {:else if parsed.kind === "success"}
      {#if REGISTRY[parsed.toolName]}
        {@const Renderer = REGISTRY[parsed.toolName]}
        <Renderer result={parsed.result} />
      {:else if shaped}
        {#if shaped.title}<div class="text-xs opacity-60">
            {shaped.title}
          </div>{/if}
        <FieldList rows={shaped.rows} />
        {#if shaped.labels}<div class="pt-1">
            <ChipRow items={shaped.labels} />
          </div>{/if}
      {:else}
        <GenericResult data={parsed.result} />
      {/if}
    {:else if parsed.kind === "generic"}
      <GenericResult data={parsed.data} />
    {:else}
      <GenericResult data={parsed.text} />
    {/if}
  </div>
</details>
