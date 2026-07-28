<script lang="ts">
  // The shared conversation render body used by every chat page: a scrollable
  // transcript (date/time markers, role bubbles, sending pulse, copy affordance,
  // inline turn-error) plus the composer, with stick-to-tail auto-scroll owned
  // internally. Page-specific chrome (status header, transport-offline banner,
  // no-channel empty state) stays in the page, which composes this as siblings
  // inside its own flex column.
  import Composer from "./Composer.svelte";
  import Markdown from "./Markdown.svelte";
  import CopyButton from "./CopyButton.svelte";
  import { createAutoScroll } from "../lib/transcript.svelte.ts";
  import { timelineMarkers } from "../lib/channel/timeline.ts";
  import type {
    ConversationLine,
    ConversationTranscript,
  } from "../lib/transcript.ts";
  import type { ComposerModel } from "../lib/composer.svelte.ts";
  import type { Snippet } from "svelte";

  let {
    lines,
    status,
    error,
    composer,
    disabled,
    pendingCount,
    notice,
  }: {
    lines: ConversationLine[];
    status: ConversationTranscript["status"];
    error?: string;
    composer: ComposerModel;
    disabled: boolean;
    pendingCount: number;
    /** Optional page-supplied notice rendered inside the scrollable transcript
     *  (after the inline error), so it scrolls with the messages. Used for the
     *  online transport-offline banner. */
    notice?: Snippet;
  } = $props();

  // busy drives the empty-state gate (only show "send a message" when idle and
  // empty); the inline error line renders when status === "error".
  const busy = $derived(status === "busy");
  // Per-line date divider / time label: a new group on day change or a >5min
  // gap; otherwise consecutive lines share the previous group's timestamp.
  const markers = $derived(timelineMarkers(lines));

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();
  $effect(() => {
    void lines.length;
    scroll.stick();
  });
</script>

<div
  class="flex-1 min-h-0 overflow-auto"
  bind:this={scroll.viewport}
  onscroll={scroll.onScroll}
>
  <div class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3">
    {#if lines.length === 0 && !busy}
      <p class="text-sm opacity-60 text-center mt-8">
        Send a message to start the conversation.
      </p>
    {/if}
    {#each lines as line, i (line.historyId)}
      {@const m = markers[i]}
      {#if m?.dateDivider}
        <div
          class="self-center text-xs opacity-50 my-2 text-center max-w-[80%]"
        >
          {m.dateDivider}{#if m.timeLabel}
            <span class="opacity-70">· {m.timeLabel}</span>{/if}
        </div>
      {:else if m?.timeLabel}
        <div class="self-center text-xs opacity-50 mt-2 text-center">
          {m.timeLabel}
        </div>
      {/if}
      {#if line.role === "system"}
        <p
          class="text-xs opacity-60 text-center whitespace-pre-wrap break-words"
        >
          {line.text}
        </p>
      {:else}
        <div
          class="group flex flex-col {line.role === 'user'
            ? 'items-end'
            : 'items-start'}"
        >
          <div
            class="max-w-[80%] min-w-0 rounded-base px-3 py-2 text-sm {line.role ===
            'user'
              ? 'preset-filled-primary-500 whitespace-pre-wrap break-words'
              : 'preset-tonal-surface'} {line.status === 'sending'
              ? 'opacity-60'
              : ''}"
          >
            {#if line.role === "assistant"}
              <Markdown source={line.text} />
            {:else}
              {line.text}
            {/if}
          </div>
          {#if line.role === "user" && line.status === "sending"}
            <span class="text-xs opacity-50 animate-pulse" aria-label="sending"
              >··</span
            >
          {:else}
            <CopyButton getText={() => line.text} />
          {/if}
        </div>
      {/if}
    {/each}
    {#if status === "error" && error}
      <p class="text-xs text-error-500 text-center">
        {error}
      </p>
    {/if}
    {@render notice?.()}
  </div>
</div>
<Composer {composer} {disabled} {pendingCount} />
