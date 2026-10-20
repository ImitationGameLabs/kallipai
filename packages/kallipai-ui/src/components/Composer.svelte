<script lang="ts">
  import { ArrowUp, Paperclip } from "@lucide/svelte";
  import type { Snippet } from "svelte";
  import type { ComposerModel } from "../lib/composer.svelte";
  import { composerKeyIntent } from "../lib/composerKeys.ts";
  import { MENTION_LIST_ID } from "../lib/mentions.ts";
  import {
    composer_placeholder,
    composer_message_aria,
    composer_send_aria,
    composer_queued,
    composer_connect_link,
    composer_connect_tail,
    chat_attach_file_aria,
  } from "../paraglide/messages.js";

  let {
    composer,
    disabled,
    pendingCount,
    disabledNotice,
    fileButton,
    attachmentBar,
    quoteBar,
    mentionBar,
    mentionPicker,
    onComposeKey,
    mentionAria,
    onSubmitted,
    onCancelQuote,
    onResized,
  }: {
    composer: ComposerModel;
    disabled: boolean;
    pendingCount: number;
    // Optional copy shown under a disabled composer. When omitted, the default
    // tagma-pairing notice ("Connect a tagma to send" + /connect link) is shown
    // -- correct for the bilateral (offline-tagma) chat surface, wrong for a
    // room error, so the room page passes a room-appropriate string here.
    disabledNotice?: string;
    // Optional snippet rendered just under the input card (the attachment
    // bar). The page owns the items and handlers; the composer only
    // places it.
    attachmentBar?: Snippet;
    // Optional file-attach affordance. When omitted (the default) the
    // button never renders -- the room page's shape (zero half-built
    // semantics where file sending is out of scope). The page owns the
    // picked files and the upload flow; the composer only opens the picker.
    fileButton?: { onFilesPicked: (files: File[]) => void };
    // Optional submit-side hook: fired synchronously when the user
    // triggers a send, before the model submit's transport await. The
    // host wires it to its auto-scroll controller so an outbound send
    // returns the transcript to the tail.
    onSubmitted?: () => void;
    // Fired after each auto-height settle: the host routes it to the
    // scroll controller so a following transcript re-anchors (the
    // measure-collapse below can clamp a pinned scrollTop against
    // the transient layout).
    onResized?: () => void;
    // Optional strip rendered above the input card (the pending quote). The
    // page owns the quotation state; the composer only places the strip.
    quoteBar?: Snippet;
    // Escape in the field drops the pending quote. The caret stays put, so
    // the key never costs the user typed text.
    onCancelQuote?: () => void;
    // Optional strip above the input card listing the members picked for
    // this send. Its own strip, not part of the quote's: a send can carry
    // both, and each is spent independently.
    mentionBar?: Snippet;
    // Optional candidate popup. It floats above the input card, so opening
    // it never reflows the transcript behind it.
    mentionPicker?: Snippet;
    // Optional last look at a key press, taken before the submit/IME rule
    // below: the mention picker's arrows and Enter belong to it, not to the
    // send path. Returning true means the hook consumed the event.
    onComposeKey?: (event: KeyboardEvent) => boolean;
    // What the field announces about the candidate popup: whether it is
    // showing, and which option `aria-activedescendant` points at. One
    // object, so the two cannot drift apart.
    mentionAria?: {
      readonly open: boolean;
      readonly activeId: string | undefined;
    };
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
  // Place the caret a request asked for. Accepting a candidate rewrites the
  // draft, and a rewritten field puts the caret at its end; this puts it back
  // where the token started. Post-flush for the same reason as the focus
  // effect: the value has to have landed before the range is set.
  $effect(() => {
    const token = composer.caretToken;
    const position = composer.caretRequest;
    if (token === 0 || position === undefined) return;
    area?.focus();
    area?.setSelectionRange(position, position);
    composer.caret = position;
  });
  // Report the caret as it moves. The mention picker decides from the draft
  // plus where the caret sits, so moves that change no text -- arrows,
  // clicks, selecting -- are read here too, not only on input.
  function trackCaret(): void {
    if (area) composer.caret = area.selectionStart;
  }
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
    onResized?.();
  }

  // The single submit entry: the hook lands in the synchronous segment
  // (before submit()'s internal await), so a conversation switch inside
  // the transport window cannot be dragged by a late scroll from the
  // old conversation -- the reset the switch runs always wins.
  function submitAtTail(): void {
    // The button's disabled gate, applied to every trigger route: a keystroke
    // that would leave the button disabled is not a send, so the hook must
    // not drag the transcript to the tail for a no-op.
    if (!composer.canSend || composer.sending) return;
    onSubmitted?.();
    void composer.submit();
  }
  // Which key does what delegates to `composerKeyIntent`, where the layout
  // and IME rules live and are unit-tested.
  function onKeydown(event: KeyboardEvent) {
    if (onComposeKey?.(event)) return;
    const intent = composerKeyIntent(event, desktop);
    if (intent === "cancel-quote") {
      onCancelQuote?.();
      return;
    }
    if (intent === "submit") {
      event.preventDefault();
      submitAtTail();
    }
  }
</script>

{#snippet sendButton()}
  <button
    type="button"
    onclick={() => submitAtTail()}
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

{#snippet fileButtonSnippet()}
  {#if fileButton}
    <!-- Hidden input + label button: the label forwards the click, the
         input resets after each pick so re-selecting the same file
         re-fires change. -->
    <input
      id="composer-file-input"
      type="file"
      multiple
      class="hidden"
      onchange={(e) => {
        const input = e.currentTarget;
        if (input.files?.length) fileButton.onFilesPicked([...input.files]);
        input.value = "";
      }}
    />
    <button
      type="button"
      class="size-10 shrink-0 rounded-full preset-tonal-surface flex items-center justify-center opacity-80 hover:opacity-100"
      aria-label={chat_attach_file_aria()}
      onclick={(e) => {
        e.preventDefault();
        document.getElementById("composer-file-input")?.click();
      }}
    >
      <Paperclip class="size-5" aria-hidden="true" />
    </button>
  {/if}
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
    <!-- The pending quote sits above the whole input card, so it reads as
         state of the send area rather than part of the typed message. -->
    {#if quoteBar}{@render quoteBar()}{/if}
    {#if mentionBar}{@render mentionBar()}{/if}
    {#if mentionPicker}
      <!-- A zero-height anchor: the popup hangs off its top edge, so it
           opens upward over the strips without reflowing anything. -->
      <div class="relative">{@render mentionPicker()}</div>
    {/if}
    <div class="flex items-end gap-2 md:block">
      <div
        class="flex-1 min-w-0 rounded-2xl border-2 border-surface-300-700 shadow-sm p-1 md:p-2 transition hover:shadow-xl focus-within:border-surface-400-600"
      >
        <textarea
          id="composer-input"
          bind:this={area}
          bind:value={composer.draft}
          onkeydown={onKeydown}
          oninput={trackCaret}
          onkeyup={trackCaret}
          onclick={trackCaret}
          onselect={trackCaret}
          role={mentionPicker ? "combobox" : undefined}
          aria-expanded={mentionPicker ? mentionAria?.open === true : undefined}
          aria-controls={mentionPicker && mentionAria?.open === true
            ? MENTION_LIST_ID
            : undefined}
          aria-activedescendant={mentionPicker && mentionAria?.open === true
            ? mentionAria.activeId
            : undefined}
          aria-autocomplete={mentionPicker ? "list" : undefined}
          placeholder={desktop ? composer_placeholder() : ""}
          rows={desktop ? 2 : 1}
          aria-label={composer_message_aria()}
          {disabled}
          class="block w-full resize-none bg-transparent border-0 outline-none focus:ring-0 px-2 pt-0.5 pb-0.5 md:pt-1.5 md:pb-2 text-base leading-relaxed"
        ></textarea>
        <!-- The action row reads as part of the input card but sits
             outside the textarea. A label aimed at the field makes
             blank-space clicks focus it natively; the mousedown guard
             only keeps the button click from stealing focus (click
             forwarding is the label's own, so the listener is not the
             interaction), hence the ignore. -->
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
        <label
          for="composer-input"
          class="hidden md:flex justify-end items-center gap-2 pt-1"
          onmousedown={(e) => e.preventDefault()}
        >
          {@render fileButtonSnippet()}
          {@render sendButton()}
        </label>
      </div>
      <div class="md:hidden shrink-0 pb-2 flex items-center gap-2">
        {@render fileButtonSnippet()}
        {@render sendButton()}
      </div>
    </div>
    {#if attachmentBar}{@render attachmentBar()}{/if}

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
