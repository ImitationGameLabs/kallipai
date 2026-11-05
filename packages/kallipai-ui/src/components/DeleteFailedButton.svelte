<script lang="ts">
  // The failed bubble's delete action: a trash button in the bubble's action
  // row that is replaced in place by a short confirm strip. In flow on
  // purpose: the question is about ONE message, which has to stay visible
  // while it is answered, and an anchored overlay flips to the opposite side
  // as the transcript runs out of room below, landing on top of that very
  // bubble. An in-line strip cannot overlap it at any width.
  //
  // Nothing is dismissed from outside: a question about an irreversible act
  // waits to be answered rather than vanishing under a stray click.
  //
  // Focus is carried by hand across the swap. Opening moves it to the strip
  // itself, so a screen reader hears the question before it hears the two
  // answers; Cancel is then one Tab away, the safe answer for a
  // destructive act. Cancelling returns focus to the trigger that owns
  // the action.
  //
  // Confirming is answered by the store's own reply, synchronously: a discard is
  // a reducer write, not a request, so the handler returns whether the line
  // left. When it did, the row and this trigger go together and the view moves
  // focus to where the reader continues -- the neighbouring row, or the
  // composer when the transcript is empty. When it did not (the line is in
  // flight, or already gone from the transcript), this trigger is still the
  // control that owns the action, and focus comes back to it.
  //
  // The trigger carries the same BUBBLE_ACTION segment as Quote and Raw, so it
  // reveals with them on hover and stays visible on touch; the open strip is
  // always visible, because it is the question.
  //
  // Chat-specific by design: it names its own question, where ConfirmDialog
  // takes the copy from its caller.
  import { Trash2 } from "@lucide/svelte";
  import {
    chat_delete_failed_aria,
    chat_delete_failed_confirm,
    common_cancel,
    common_delete,
  } from "../paraglide/messages.js";
  import { BUBBLE_ACTION } from "../lib/classes.ts";

  let {
    onConfirm,
  }: {
    /** Runs the discard and answers whether the line left. False means the
     *  line survived it (in flight, or already gone), and focus returns here. */
    onConfirm: () => boolean;
  } = $props();

  let open = $state(false);
  let triggerEl = $state<HTMLButtonElement | null>(null);
  let stripEl = $state<HTMLDivElement | null>(null);
  let wasOpen = false;
  /** Set on the confirming path only: the close must not pull focus back to
   *  the trigger before the store has answered. */
  let confirming = false;

  const questionId = $props.id();

  $effect(() => {
    if (open === wasOpen) return;
    wasOpen = open;
    if (open) {
      // The strip is taller than the trigger it replaces, and the transcript
      // does not re-anchor on a height change, so bring it into view by the
      // smallest move that works. Focus scrolls too, but by its own rule --
      // centred when it judges that necessary -- so it is told not to. The
      // "no-op when already visible" property belongs to scrollIntoView, not
      // to focus, which is exactly why the two are separated here.
      confirming = false;
      stripEl?.focus({ preventScroll: true });
      stripEl?.scrollIntoView({ block: "nearest" });
    } else if (!confirming) {
      triggerEl?.focus();
    }
  });

  function cancel(): void {
    open = false;
  }

  function confirm(): void {
    confirming = true;
    open = false;
    // The answer decides the focus, and the store answers now, not later: a
    // refusal means the line is still there and this trigger still owns the
    // action. A discard places focus itself, inside the same synchronous turn.
    if (!onConfirm()) triggerEl?.focus();
  }

  /** Escape backs out from anywhere inside the strip, because the keydown
   *  bubbles to the container from either button. Deliberately not a
   *  window-level listener: that would take Escape from the rest of the page.
   *  Once focus leaves the strip, Escape is not heard here and the strip stays
   *  open -- a stated boundary, not an oversight. */
  function onKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape") cancel();
  }
</script>

{#if open}
  <!-- The border is a step darker than the page hairline instead of a shadow:
       the strip sits in the transcript, where a soft shadow on a dark page
       reads as a smudge. -->
  <!-- The strip is a labelled group, not a control: the keydown it listens
       for is Escape, which only ever backs out, and the answers are the two
       buttons inside it. -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div
    bind:this={stripEl}
    tabindex="-1"
    onkeydown={onKeydown}
    role="group"
    aria-labelledby={questionId}
    class="flex flex-col gap-1.5 rounded-base border border-surface-300-700 bg-surface-100-900 p-2 text-sm"
  >
    <span id={questionId} class="font-medium">
      {chat_delete_failed_confirm()}
    </span>
    <div class="flex gap-2 self-end">
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        onclick={cancel}
      >
        {common_cancel()}
      </button>
      <button
        type="button"
        class="btn btn-sm border border-transparent preset-filled-error-500 text-on-error-500 transition hover:brightness-110"
        onclick={confirm}
      >
        {common_delete()}
      </button>
    </div>
  </div>
{:else}
  <button
    type="button"
    bind:this={triggerEl}
    class={BUBBLE_ACTION}
    title={common_delete()}
    aria-label={chat_delete_failed_aria()}
    onclick={() => (open = true)}
  >
    <Trash2 class="size-4" aria-hidden="true" />
  </button>
{/if}
