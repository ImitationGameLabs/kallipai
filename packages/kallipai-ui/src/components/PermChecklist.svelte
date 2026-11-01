<script lang="ts">
  // The permission picker: the strings this build knows as checkboxes, plus any
  // stored string it does not know, shown raw so a server that grew a seventh
  // permission never has its grant dropped here.
  import { common_remove } from "../paraglide/messages.js";
  import {
    PERM_IDS,
    permLabel,
    toggleStored,
    type PermId,
  } from "../lib/perms.ts";

  let {
    value,
    onChange,
    unknown = [],
    ids = PERM_IDS,
    disabled = false,
  }: {
    value: readonly string[];
    onChange: (next: string[]) => void;
    unknown?: readonly string[];
    ids?: readonly PermId[];
    disabled?: boolean;
  } = $props();
</script>

<div class="grid gap-1 sm:grid-cols-2">
  {#each ids as p (p)}
    <label class="flex items-start gap-2 text-xs">
      <input
        type="checkbox"
        class="checkbox mt-0.5"
        checked={value.includes(p)}
        onchange={() => onChange(toggleStored(value, p))}
        {disabled}
      />
      <span>{permLabel(p)}</span>
    </label>
  {/each}
  {#each unknown as p (p)}
    <span
      class="flex items-center gap-1 text-xs font-mono opacity-60 break-all"
    >
      {p}
      {#if !disabled}
        <button
          type="button"
          class="link text-xs"
          onclick={() => onChange(value.filter((v) => v !== p))}
          aria-label="{common_remove()}: {p}">×</button
        >
      {/if}
    </span>
  {/each}
</div>
