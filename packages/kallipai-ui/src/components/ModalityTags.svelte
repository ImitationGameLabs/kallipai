<script lang="ts">
  // The shared modality tag group (the manage ProfileDialog pattern):
  // one badge per wire modality in canonical order, `text` pinned as a
  // permanent badge (text-only is the server default, so it is never a
  // toggle) and the rest as pressed-state toggles. Controlled: the
  // caller owns the selection and applies the flipped set through
  // onChange; the label is i18n at the call site.
  import { MODALITY_ORDER } from "../lib/manage/compute.ts";
  import type { Modality } from "@kallipai/kallipai-client";

  let {
    selected,
    label,
    onChange,
  }: {
    /** The selected modalities (text is always in effect). */
    selected: readonly Modality[];
    /** The group's accessible name (i18n at the call site). */
    label: string;
    /** The flipped selection after a toggle. */
    onChange: (next: readonly Modality[]) => void;
  } = $props();

  function toggle(m: Modality): void {
    // The selection stays in canonical order (the manage dialog's
    // flip semantics: rebuild from MODALITY_ORDER on add).
    const next = selected.includes(m)
      ? selected.filter((x) => x !== m)
      : MODALITY_ORDER.filter((x) => x === m || selected.includes(x));
    onChange(next);
  }
</script>

<div class="flex flex-col gap-1">
  <span class="text-sm font-medium">{label}</span>
  <div class="flex flex-wrap gap-1.5" role="group" aria-label={label}>
    {#each MODALITY_ORDER as m (m)}
      {#if m === "text"}
        <span class="badge rounded-full text-xs preset-filled-primary-500">
          {m}
        </span>
      {:else}
        <button
          type="button"
          aria-pressed={selected.includes(m)}
          class="badge rounded-full text-xs cursor-pointer transition {selected.includes(
            m,
          )
            ? 'preset-filled-primary-500'
            : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
          onclick={() => toggle(m)}
        >
          {m}
        </button>
      {/if}
    {/each}
  </div>
</div>
