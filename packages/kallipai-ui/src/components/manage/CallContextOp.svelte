<script lang="ts">
  // context_pin and context_unpin share a body: which label the call named,
  // and what is pinned afterwards. Every such reply carries the whole pinned
  // set, so the chips show the store's list rather than the page's count.
  // When the call unpinned the last label the set is empty, and the sentence
  // alone says so.
  import type {
    CallViewProps,
    ContextPinResult,
    ContextUnpinResult,
  } from "../../lib/manage/toolViews.ts";
  import { contextOpFacts } from "../../lib/manage/toolViews.ts";
  import {
    manage_turns_pin_done,
    manage_turns_unpin_done,
  } from "../../paraglide/messages.js";
  import CallNarration from "./CallNarration.svelte";

  let { name, payload }: CallViewProps<ContextPinResult | ContextUnpinResult> =
    $props();

  const facts = $derived(contextOpFacts(name, payload));

  /** Both outcomes are the call doing what it was asked, so the dot reports a
   * success either way: the verb is in the sentence. */
  const tone = "ok";

  const line = $derived(
    facts.verb === "pinned"
      ? manage_turns_pin_done({ label: facts.label, count: `${facts.total}` })
      : manage_turns_unpin_done({
          label: facts.label,
          count: `${facts.total}`,
        }),
  );
</script>

<div class="space-y-1">
  <CallNarration {tone} text={line} />
  {#if facts.labels.length > 0}
    <div class="flex flex-wrap items-center gap-1 pl-3.5">
      {#each facts.labels as label}
        <span
          class="rounded-sm bg-surface-50-950 px-1.5 py-0.5 font-mono text-xs ring-1 ring-surface-200-800"
        >
          {label}
        </span>
      {/each}
    </div>
  {/if}
</div>
