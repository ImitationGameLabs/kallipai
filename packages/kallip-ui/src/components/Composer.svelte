<script lang="ts">
  import { ArrowUp, Square } from "@lucide/svelte";
  import type { ComposerModel } from "../lib/composer.svelte";

  let {
    composer,
    disabled,
    busy,
    pendingCount,
    oninterrupt,
  }: {
    composer: ComposerModel;
    disabled: boolean;
    busy: boolean;
    pendingCount: number;
    oninterrupt: () => void;
  } = $props();

  let area: HTMLTextAreaElement | undefined = $state();

  // Auto-grow: recompute on every draft change so programmatic writes (e.g. an
  // empty-state prompt chip) also resize, not just user key strokes.
  $effect(() => {
    void composer.draft;
    resize();
  });

  // Honour focus requests from external triggers (prompt chips). Skips the
  // initial mount run (focusToken starts at 0) so the field does not steal
  // focus on page load or navigation.
  $effect(() => {
    const token = composer.focusToken;
    if (token > 0) area?.focus();
  });

  function resize() {
    if (!area) return;
    area.style.height = "auto";
    // Cap at roughly ten rows before the field scrolls internally.
    area.style.height = `${Math.min(area.scrollHeight, 240)}px`;
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void composer.submit();
    }
  }

  // The single circular action: sends when idle, interrupts when busy (the icon
  // swaps to a stop square, Claude-style).
  function onAction() {
    if (busy) oninterrupt();
    else void composer.submit();
  }
</script>

<div class="pt-3 px-3 pb-6">
  <div class="max-w-3xl mx-auto">
    <!-- Input card: one bordered frame holds the textarea + the action, so the
         textarea itself is borderless/transparent and the card outline is the
         sole edge. focus-within retints the border to signal the active field. -->
    <div
      class="rounded-2xl border-2 border-surface-300 shadow-sm p-2 transition hover:shadow-xl focus-within:border-surface-500"
    >
      <textarea
        bind:this={area}
        bind:value={composer.draft}
        onkeydown={onKeydown}
        placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
        rows="2"
        aria-label="Message"
        {disabled}
        class="block w-full resize-none bg-transparent border-0 outline-none focus:ring-0 px-2 pt-1.5 pb-2 text-base leading-relaxed"
      ></textarea>
      <div class="flex justify-end pt-1">
        <button
          type="button"
          onclick={onAction}
          disabled={!busy && !composer.canSend}
          aria-label={busy ? "Interrupt" : "Send"}
          class="size-9 shrink-0 rounded-full preset-filled-primary-500 flex items-center justify-center disabled:opacity-40 disabled:cursor-not-allowed"
        >
          {#if busy}
            <Square class="size-4" aria-hidden="true" />
          {:else}
            <ArrowUp class="size-5" aria-hidden="true" />
          {/if}
        </button>
      </div>
    </div>

    {#if pendingCount > 0}
      <div class="mt-1.5 text-xs opacity-60">
        <span class="badge preset-tonal-surface">queued: {pendingCount}</span>
      </div>
    {:else if disabled}
      <div class="mt-1.5 text-xs opacity-60">
        <a
          href="/connect"
          class="font-medium text-primary-500 hover:underline cursor-pointer"
          >Connect a daemon</a
        >
        to send.
      </div>
    {/if}
  </div>
</div>
