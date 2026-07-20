<script lang="ts">
  import type { TranscriptLine } from "@kallipai/kallip-common";
  import { createAutoScroll } from "../lib/transcript.svelte";
  import type { ComposerModel } from "../lib/composer.svelte";
  import Markdown from "./Markdown.svelte";
  import Brand from "./Brand.svelte";
  import ToolCallCard from "./tools/ToolCallCard.svelte";
  import ToolResultCard from "./tools/ToolResultCard.svelte";

  let {
    lines,
    composer,
  }: { lines: TranscriptLine[]; composer: ComposerModel } = $props();

  // Tail-following scroll: pinned to the bottom while lines arrive, unless the
  // user scrolled up. Driven by an effect that reads the line count.
  const scroll = createAutoScroll();
  $effect(() => {
    void lines.length;
    scroll.stick();
  });

  // First-touch prompts: at this point there may be no project bound yet (e.g.
  // a fresh cloud deploy), so these stay context-free and conversational rather
  // than presuming a codebase or jumping into dev tasks.
  const examplePrompts = [
    "Hi! What can we work on?",
    "What are you able to do?",
    "Help me get set up",
  ];

  function usePrompt(text: string) {
    composer.draft = text;
    composer.requestFocus();
  }
</script>

<div
  bind:this={scroll.viewport}
  onscroll={scroll.onScroll}
  class="h-full overflow-y-auto p-4"
>
  {#if lines.length === 0}
    <!-- Empty-state hero: only meaningful once a session is attached. -->
    <div
      class="h-full flex flex-col items-center justify-center text-center gap-6 py-10"
    >
      <Brand size="xl" />
      <p class="text-base opacity-60 max-w-md">
        How can I help? Try one of these, or write your own below.
      </p>
      <div class="flex flex-wrap justify-center gap-3 max-w-md">
        {#each examplePrompts as prompt (prompt)}
          <button
            type="button"
            onclick={() => usePrompt(prompt)}
            class="btn preset-tonal-primary hover:preset-filled-primary-500 rounded-full transition hover:scale-105 hover:shadow-md"
          >
            {prompt}
          </button>
        {/each}
      </div>
    </div>
  {:else}
    <!--
      Centered column: lines share one max-width so the transcript aligns with
      the composer below. Keyed by array index, which is safe because the
      transcript reducer is append-only (it never reorders/removes/mutates in
      place); if that ever changes, switch to a stable per-line id.
    -->
    <div class="max-w-3xl mx-auto space-y-3">
      {#each lines as line, i (i)}
        {#if line.kind === "user"}
          <div class="flex justify-end">
            <div class="max-w-[80%]">
              <div
                class="text-right text-xs uppercase tracking-wide opacity-40 mb-1"
              >
                you
              </div>
              <div
                class="rounded-lg px-3 py-2 preset-tonal-surface whitespace-pre-wrap text-sm"
              >
                {line.text}
              </div>
            </div>
          </div>
        {:else if line.kind === "assistant"}
          <div class="border-l-2 border-primary-500 pl-3">
            <div class="text-xs uppercase tracking-wide opacity-40 mb-1">
              assistant
            </div>
            {#if line.streaming}
              <!-- While streaming, render plain text + caret; switch to markdown
                   once finalized to avoid re-parsing per delta and half-rendered
                   fences. -->
              <div class="whitespace-pre-wrap text-sm">
                {line.text}<span class="opacity-50 animate-pulse">▋</span>
              </div>
            {:else}
              <div class="text-sm">
                <Markdown source={line.text} />
              </div>
            {/if}
          </div>
        {:else if line.kind === "reasoning"}
          <details open={line.streaming} class="opacity-70">
            <summary
              class="text-xs uppercase tracking-wide opacity-50 cursor-pointer select-none"
            >
              thinking
            </summary>
            <div class="whitespace-pre-wrap text-xs italic pl-1">
              {line.text}{#if line.streaming}<span>▋</span>{/if}
            </div>
          </details>
        {:else if line.kind === "toolCall"}
          <ToolCallCard {line} />
        {:else if line.kind === "toolResult"}
          <ToolResultCard {line} />
        {:else if line.kind === "error"}
          <div class="text-sm text-error-500">[error] {line.text}</div>
        {:else if line.kind === "status"}
          <div class="text-xs opacity-60 italic">{line.text}</div>
        {:else if line.kind === "system"}
          <div class="text-xs opacity-50">[system] {line.text}</div>
        {:else if line.kind === "retrying"}
          <div class="text-xs opacity-60">
            retrying ({line.attempt}/{line.maxAttempts}) in {line.delaySecs}s: {line.error}
          </div>
        {:else if line.kind === "failover"}
          <div class="text-xs text-warning-500">
            failover {line.from} → {line.to}: {line.reason}
          </div>
        {:else if line.kind === "failoverExhausted"}
          <div class="space-y-0.5">
            <div class="text-xs text-error-500">
              failover exhausted: {line.reason}
            </div>
            {#if line.detail}
              <div class="text-xs opacity-60">
                {line.detail}
              </div>
            {/if}
          </div>
        {:else if line.kind === "streamDropped"}
          <div class="text-xs text-warning-500">
            stream dropped, retrying ({line.attempt}/{line.maxAttempts}) in
            {line.delaySecs}s: {line.error}
          </div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
