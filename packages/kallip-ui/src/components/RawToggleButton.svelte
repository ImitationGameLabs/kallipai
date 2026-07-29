<script lang="ts">
  // Per-message toggle between rendered markdown and raw source text. Mirrors
  // CopyButton's class set and hover-reveal contract (faint on touch, hover
  // reveal on desktop via the surrounding `.group`). The current mode is shown
  // by swapping the icon -- CodeXml ("view source") while rendered, Eye
  // ("view rendered") while raw -- rather than by recoloring, so there is no
  // same-property class conflict and the state reads at a glance.
  import { CodeXml, Eye } from "@lucide/svelte";

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
  const label = $derived(pressed ? "Show rendered markdown" : "Show raw text");
</script>

<button
  type="button"
  {onclick}
  aria-pressed={pressed}
  title={label}
  aria-label={label}
  class="rounded p-1.5 text-surface-500 opacity-60 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-hover:opacity-100 focus-visible:opacity-100 hover:bg-surface-200 transition {klass}"
>
  {#if pressed}
    <Eye class="size-4" />
  {:else}
    <CodeXml class="size-4" />
  {/if}
</button>
