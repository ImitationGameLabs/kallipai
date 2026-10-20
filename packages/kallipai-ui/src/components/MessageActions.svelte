<script lang="ts">
  // The per-message affordances a message row shows beside its bubble: a direct
  // "quote" button and the shared overflow menu carrying the same action. The
  // row is hover/focus revealed on pointer devices and always visible on touch
  // -- there the parent opens the SAME menu on a long press, so touch users
  // reach quoting without a hover state that does not exist.
  //
  // Both the open flag and the action are parent-owned: one menu can be open per
  // transcript, so the state cannot live in each row, and the parent hands the
  // quote request to the quotes controller, which owns which line and preview.
  // menu surface itself come from the shared ActionMenu.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import { Quote } from "@lucide/svelte";
  import ActionMenu from "./ActionMenu.svelte";
  import { MENU_ITEM } from "../lib/classes.ts";
  import {
    chat_more_actions_aria,
    chat_quote_action,
    chat_quote_action_aria,
  } from "../paraglide/messages.js";

  let {
    menuOpen,
    onMenuOpenChange,
    onQuote,
  }: {
    /** Whether this row's overflow menu is open (parent holds the single
     *  open-menu id for the transcript). */
    menuOpen: boolean;
    onMenuOpenChange: (open: boolean) => void;
    onQuote: () => void;
  } = $props();

  // Hover reveal for pointer devices; an open menu keeps the pair visible, so
  // the trigger does not fade out from under the pointer that just opened it.
  const VISIBILITY = $derived(
    menuOpen
      ? "opacity-100"
      : "[@media(hover:hover)]:opacity-0 group-hover:opacity-100 group-focus-within:opacity-100",
  );
  const BUTTON =
    "rounded p-1.5 text-surface-500 dark:text-surface-400 opacity-60 hover:bg-surface-200-800 hover:opacity-100 focus-visible:opacity-100 cursor-pointer transition";
</script>

<div class="flex items-center gap-1 {VISIBILITY}">
  <button
    type="button"
    class={BUTTON}
    aria-label={chat_quote_action_aria()}
    title={chat_quote_action()}
    onclick={() => onQuote()}
  >
    <Quote class="size-4" aria-hidden="true" />
  </button>
  <ActionMenu
    label={chat_more_actions_aria()}
    triggerClass={BUTTON}
    open={menuOpen}
    onOpenChange={onMenuOpenChange}
    onSelect={() => onQuote()}
  >
    <Menu.Item value="quote" class={MENU_ITEM}>
      <Quote class="size-3.5" aria-hidden="true" />
      <span>{chat_quote_action()}</span>
    </Menu.Item>
  </ActionMenu>
</div>
