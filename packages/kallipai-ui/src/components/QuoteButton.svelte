<script lang="ts">
  // The per-message quote entry point: one button in the row under the bubble,
  // beside the copy and raw toggles. It owns no state -- the surface decides
  // what quoting means and whether the row offers it -- so `onclick` is wired
  // by whoever renders the row.
  //
  // Same BUBBLE_ACTION segment as CopyButton and RawToggleButton, so the three
  // reveal together on hover and stay visible on touch.
  import { Quote } from "@lucide/svelte";
  import {
    chat_quote_action,
    chat_quote_action_aria,
  } from "../paraglide/messages.js";
  import { BUBBLE_ACTION } from "../lib/classes.ts";

  let {
    onclick,
  }: {
    onclick: () => void;
  } = $props();
</script>

<button
  type="button"
  {onclick}
  title={chat_quote_action()}
  aria-label={chat_quote_action_aria()}
  class={BUBBLE_ACTION}
>
  <Quote class="size-4" aria-hidden="true" />
</button>
