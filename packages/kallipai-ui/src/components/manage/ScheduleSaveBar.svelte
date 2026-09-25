<script lang="ts">
  // The sticky save bar: the unsaved chip, the unrepresentable warning,
  // and the discard/save buttons. Pure presentational — dirty, canSave,
  // and the save/discard flows stay with the page (the draft owner).
  import {
    common_save,
    manage_profiles_discard,
  } from "../../paraglide/messages.js";
  import {
    manage_schedules_unsaved,
    manage_schedules_unrepresentable,
  } from "../../paraglide/messages.js";

  let {
    isSaving,
    representable,
    canSave,
    onDiscard,
    onSave,
  }: {
    isSaving: boolean;
    representable: boolean;
    canSave: boolean;
    onDiscard: () => void;
    onSave: () => void;
  } = $props();
</script>

<div
  class="flex flex-wrap items-center gap-3 sticky bottom-0 rounded-xl border border-warning-200-800 preset-tonal-warning px-4 py-3"
>
  <span class="chip preset-outlined-warning-500 text-xs font-medium">
    <span class="size-2 rounded-full bg-warning-500" aria-hidden="true"></span>
    {manage_schedules_unsaved()}</span
  >
  <!-- The error text is breakable CJK: left unwrapped it
       collapses to ~1 char per line under flex shrink (the
       rigid chip + buttons consume the rest), so it owns its
       own full row below md and goes inline from md up. -->
  {#if !representable}
    <span
      class="text-xs text-error-500 dark:text-error-400 basis-full md:basis-auto min-w-0"
    >
      {manage_schedules_unrepresentable()}
    </span>
  {/if}
  <div class="ml-auto flex gap-3 shrink-0">
    <button
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
      disabled={isSaving}
      onclick={onDiscard}
    >
      {manage_profiles_discard()}
    </button>
    <button
      class="btn btn-sm preset-filled-primary-200-800"
      disabled={!canSave}
      onclick={onSave}
    >
      {common_save()}
    </button>
  </div>
</div>
