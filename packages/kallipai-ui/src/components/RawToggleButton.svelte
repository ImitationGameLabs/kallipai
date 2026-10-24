<script lang="ts">
  // Per-message toggle between rendered markdown and raw source text. Mirrors
  // CopyButton's class set and hover-reveal contract (faint on touch, hover
  // reveal on desktop via the surrounding `.group`). The current mode is shown
  // by swapping the icon -- CodeXml ("view source") while rendered, Eye
  // ("view rendered") while raw -- rather than by recoloring, so there is no
  // same-property class conflict and the state reads at a glance.
  import { CodeXml, Eye } from "@lucide/svelte";
  import {
    chat_show_raw_text,
    chat_show_rendered_markdown,
  } from "../paraglide/messages.js";
  import { BUBBLE_ACTION } from "../lib/classes.ts";

  let {
    pressed,
    onclick,
    class: klass = "",
  }: {
    pressed: boolean;
    onclick: () => void;
    class?: string;
  } = $props();

  // Label names the next action (matches CopyButton's "Copy"/"Copied" style).
  const label = $derived(
    pressed ? chat_show_rendered_markdown() : chat_show_raw_text(),
  );
</script>

<button
  type="button"
  {onclick}
  aria-pressed={pressed}
  title={label}
  aria-label={label}
  class="{BUBBLE_ACTION} {klass}"
>
  {#if pressed}
    <Eye class="size-4" />
  {:else}
    <CodeXml class="size-4" />
  {/if}
</button>
