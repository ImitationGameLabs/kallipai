<script lang="ts">
  // The follow control: one bell, the rule in force in words, and a click that
  // moves it. It reads the shared standing cache (the room page's preview reads
  // the same table) and writes through it.
  //
  // The click is not applied optimistically. The state a click lands on after
  // dropping the choice is the server's default for this reader (a human lands
  // on following, an agent on involved), and guessing it is exactly the mistake
  // the control exists to prevent: the button is held while the write is out
  // and shows what the route answered.
  //
  // Nothing renders until the standing is known: a control that showed a rule
  // before reading one would be announcing the default of the wrong reader.
  //
  // The accessible name is the rule and then the click, in that order, because
  // the rule is the visible word: a reader who says what they see must hit this
  // button, and a name that only spells the action would miss it.
  import { untrack } from "svelte";
  import { Bell, BellOff, BellRing } from "@lucide/svelte";
  import { room_thread_choice_failed } from "../../paraglide/messages.js";
  import { threadChoiceDisplay } from "../../lib/rooms/threadChoice.ts";
  import {
    clearThreadChoice,
    ensureThreadChoice,
    setThreadChoice,
    threadChoiceOf,
  } from "../../lib/rooms/threadChoices.svelte.ts";

  let {
    roomId,
    root,
  }: {
    /** The room the thread lives in. */
    roomId: string;
    /** The thread's head position. */
    root: number;
  } = $props();

  const standing = $derived(threadChoiceOf(roomId, root));
  const display = $derived(
    standing === null ? null : threadChoiceDisplay(standing.effective),
  );

  let busy = $state(false);
  /** The thread whose last write failed: a failure stays with the thread that
   *  met it, never follows the reader to the next one. */
  let failedIn = $state<string | null>(null);
  const key = $derived(`${roomId}:${root}`);

  // Read the standing of whatever thread this control is on. Nothing in the
  // cache is tracked: a read of it would re-run this effect when the answer
  // lands, and the answer is what the effect just asked for.
  $effect(() => {
    const room = roomId;
    const head = root;
    untrack(() => {
      void ensureThreadChoice(room, head);
    });
  });

  async function choose(): Promise<void> {
    if (display === null || busy) return;
    busy = true;
    failedIn = null;
    try {
      if (display.next === null) await clearThreadChoice(roomId, root);
      else await setThreadChoice(roomId, root, display.next);
    } catch {
      failedIn = key;
    } finally {
      busy = false;
    }
  }
</script>

{#if display !== null}
  <div class="flex items-center gap-2 min-w-0">
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 disabled:opacity-60"
      disabled={busy}
      aria-label={`${display.text()} · ${display.action()}`}
      onclick={choose}
    >
      {#if display.icon === "bell"}
        <Bell class="size-3.5 shrink-0" aria-hidden="true" />
      {:else if display.icon === "bell-off"}
        <BellOff class="size-3.5 shrink-0" aria-hidden="true" />
      {:else}
        <BellRing class="size-3.5 shrink-0" aria-hidden="true" />
      {/if}
      <span class="truncate">{display.text()}</span>
    </button>
    {#if failedIn === key}
      <p class="text-xs text-error-500 dark:text-error-400" role="alert">
        {room_thread_choice_failed()}
      </p>
    {/if}
  </div>
{/if}
