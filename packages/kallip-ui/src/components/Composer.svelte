<script lang="ts">
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
</script>

<div class="border-t border-surface-200 p-3">
  <div class="flex items-end gap-2">
    <textarea
      bind:this={area}
      bind:value={composer.draft}
      onkeydown={onKeydown}
      placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
      rows="1"
      aria-label="Message"
      {disabled}
      class="input flex-1 resize-none text-sm leading-relaxed"></textarea>
    {#if busy}
      <button class="btn preset-filled-error-500" onclick={oninterrupt}>
        Interrupt
      </button>
    {:else}
      <button
        class="btn preset-filled-primary-500"
        onclick={() => composer.submit()}
        disabled={!composer.canSend}
      >
        Send
      </button>
    {/if}
  </div>

  {#if pendingCount > 0}
    <div class="mt-1.5 text-xs opacity-60">
      <span class="badge preset-tonal-surface">queued: {pendingCount}</span>
    </div>
  {:else if disabled}
    <div class="mt-1.5 text-xs opacity-60">
      <a href="/settings" class="link">Connect in Settings</a> to send.
    </div>
  {/if}
</div>
