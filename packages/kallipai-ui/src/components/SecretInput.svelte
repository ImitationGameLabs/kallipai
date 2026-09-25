<script lang="ts">
  // A shared input for API keys and other non-login secrets. Deliberately
  // NOT type="password": browsers run credential heuristics on password
  // fields (autocomplete="off" does not suppress the save-password prompt),
  // and a provider key is not a website login. type="text" plus
  // autocomplete="one-time-code" keeps it out of that heuristic; masked by
  // default via -webkit-text-security (Firefox ignores it and shows plain
  // text -- accepted, see task record), with an eye toggle to reveal.
  import { Eye, EyeOff } from "@lucide/svelte";
  import {
    common_show_secret,
    common_hide_secret,
  } from "../paraglide/messages.js";

  let {
    value = $bindable(""),
    placeholder = undefined,
    disabled = false,
  }: {
    value: string;
    placeholder?: string;
    disabled?: boolean;
  } = $props();

  let revealed = $state(false);
</script>

<div class="relative w-full">
  <input
    class="input text-sm font-mono w-full"
    style={revealed ? "" : "-webkit-text-security: disc;"}
    type="text"
    autocomplete="one-time-code"
    spellcheck="false"
    {placeholder}
    {disabled}
    bind:value
  />
  <button
    type="button"
    class="absolute right-2 top-1/2 -translate-y-1/2 rounded p-0.5 text-surface-500 transition hover:bg-surface-200-800 dark:text-surface-400"
    aria-label={revealed ? common_hide_secret() : common_show_secret()}
    onclick={() => (revealed = !revealed)}
  >
    {#if revealed}
      <EyeOff class="size-4" />
    {:else}
      <Eye class="size-4" />
    {/if}
  </button>
</div>
