<script lang="ts">
  import { ArrowUp } from "@lucide/svelte";
  import type { ComposerModel } from "../lib/composer.svelte";
  import {
    composer_placeholder,
    composer_message_aria,
    composer_send_aria,
    composer_queued,
    composer_connect_link,
    composer_connect_tail,
  } from "../paraglide/messages.js";

  let {
    composer,
    disabled,
    pendingCount,
    disabledNotice,
  }: {
    composer: ComposerModel;
    disabled: boolean;
    pendingCount: number;
    // Optional copy shown under a disabled composer. When omitted, the default
    // tagma-pairing notice ("Connect a tagma to send" + /connect link) is shown
    // -- correct for the bilateral (offline-tagma) chat surface, wrong for a
    // room error, so the room page passes a room-appropriate string here.
    disabledNotice?: string;
  } = $props();

  let area: HTMLTextAreaElement | undefined = $state();

  // Auto-grow: recompute on every draft change (and breakpoint flip, below)
  // so programmatic writes (e.g. an empty-state prompt chip), rows flips and
  // padding changes all re-measure, not just user key strokes.
  $effect(() => {
    void composer.draft;
    void desktop; // post-flush: the rows attr has re-rendered before this runs
    resize();
  });

  // Honour focus requests from external triggers (prompt chips). Skips the
  // initial mount run (focusToken starts at 0) so the field does not steal
  // focus on page load or navigation.
  $effect(() => {
    const token = composer.focusToken;
    if (token > 0) area?.focus();
  });
  // Cross-breakpoint rows: below md the composer starts at one line and caps
  // at five (mobile spec); at md+ it keeps the historical two-line start
  // and 240px cap. Mirrors the AppShell mdQuery listener pattern; the
  // measure effect below re-runs on the flip (post-flush), so the rows
  // attribute and the cap are both current when the height is recomputed.
  const mdQuery = matchMedia("(min-width: 48rem)");
  let desktop = $state(mdQuery.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => {
      desktop = event.matches;
    };
    mdQuery.addEventListener("change", onChange);
    return () => mdQuery.removeEventListener("change", onChange);
  });

  function resize() {
    if (!area) return;
    // Collapse to 0 first: 'auto' re-lays out at the rows attribute
    // height, so an empty field would measure the attribute's rows and
    // never shrink.
    area.style.height = "0px";
    // Cap before the field scrolls internally: 240px (~ten lines) at
    // md+, five lines below md. scrollHeight and the computed-style math
    // both include the textarea's vertical padding, so the units agree.
    const style = getComputedStyle(area);
    const cap = desktop
      ? 240
      : 5 * Number.parseFloat(style.lineHeight) +
        Number.parseFloat(style.paddingTop) +
        Number.parseFloat(style.paddingBottom);
    area.style.height = `${Math.min(area.scrollHeight, cap)}px`;
  }

  // Enter submits at md+ (the desktop IM convention); below md Enter inserts
  // a newline instead and the send button is the only way to submit (mobile
  // keyboards pair Enter with a newline habit, so submit-on-Enter mistypes).
  function onKeydown(event: KeyboardEvent) {
    if (desktop && event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void composer.submit();
    }
  }
</script>

{#snippet sendButton()}
  <button
    type="button"
    onclick={() => void composer.submit()}
    disabled={!composer.canSend || composer.sending}
    aria-label={composer_send_aria()}
    aria-busy={composer.sending}
    class="size-10 shrink-0 rounded-full preset-filled-primary-500 flex items-center justify-center disabled:opacity-40 disabled:cursor-not-allowed"
  >
    <ArrowUp
      class="size-5 {composer.sending ? 'animate-spin' : ''}"
      aria-hidden="true"
    />
  </button>
{/snippet}

<!-- Bottom padding: 1.5rem intended breathing room, or the safe-area
     inset when larger -- minus the keyboard inset, because edge-to-edge
     maps the IME into the safe-area env on WebView and resizes-content
     already lifts the composer above the keyboard. The 0px fallback keeps
     engines without keyboard-inset-height on the original behaviour. -->
<div
  class="pt-3 px-3 pb-[max(1.5rem,calc(env(safe-area-inset-bottom)-env(keyboard-inset-height,0px)))]"
>
  <div class="max-w-3xl mx-auto">
    <!-- Input card: one bordered frame holds the textarea + the desktop
         action, so the textarea itself is borderless/transparent and the
         card outline is the sole edge. focus-within retints the border to
         signal the active field. Below md the send button moves outside
         the card (mobile composer spec): the outer row aligns it with the
         textarea's last line instead. -->
    <div class="flex items-end gap-2 md:block">
      <div
        class="flex-1 min-w-0 rounded-2xl border-2 border-surface-300-700 shadow-sm p-1 md:p-2 transition hover:shadow-xl focus-within:border-surface-400-600"
      >
        <textarea
          bind:this={area}
          bind:value={composer.draft}
          onkeydown={onKeydown}
          placeholder={desktop ? composer_placeholder() : ""}
          rows={desktop ? 2 : 1}
          aria-label={composer_message_aria()}
          {disabled}
          class="block w-full resize-none bg-transparent border-0 outline-none focus:ring-0 px-2 pt-0.5 pb-0.5 md:pt-1.5 md:pb-2 text-base leading-relaxed"
        ></textarea>
        <!-- The action row reads as part of the input card but sits
             outside the textarea, so blank-space clicks forward to the
             field and mousedown is cancelled so the button cannot steal
             focus. -->
        <div
          class="hidden md:flex justify-end pt-1"
          onmousedown={(e) => e.preventDefault()}
          onclick={(e) => {
            if (e.target === e.currentTarget) area?.focus();
          }}
        >
          {@render sendButton()}
        </div>
      </div>
      <div class="md:hidden shrink-0 pb-2">
        {@render sendButton()}
      </div>
    </div>

    {#if pendingCount > 0}
      <div class="mt-1.5 text-xs opacity-60">
        <span class="badge preset-tonal-surface"
          >{composer_queued({ count: pendingCount })}</span
        >
      </div>
    {:else if disabled}
      <div class="mt-1.5 text-xs opacity-60">
        {#if disabledNotice}
          {disabledNotice}
        {:else}
          <a
            href="/connect"
            class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
            >{composer_connect_link()}</a
          >
          {composer_connect_tail()}
        {/if}
      </div>
    {/if}
  </div>
</div>
