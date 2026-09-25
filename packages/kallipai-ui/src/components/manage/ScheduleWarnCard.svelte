<script lang="ts">
  // The warnings-and-wake card: the two preemption lead times and the
  // custom prompts. A controlled field card — values come in as props
  // and every edit leaves as one onField call with the field key, so
  // the draft (the state owner) stays in the page. The page renders
  // this card only for modes with shift boundaries.
  import {
    manage_schedules_final_warn,
    manage_schedules_final_warn_hint,
    manage_schedules_final_warn_prompt,
    manage_schedules_pre_warn,
    manage_schedules_wake_hint,
    manage_schedules_wake_prompt,
    manage_schedules_warn_order,
    manage_schedules_warnings,
  } from "../../paraglide/messages.js";

  export type WarnField = "pre" | "fin" | "wake" | "final";
  let {
    pre,
    fin,
    wakePrompt,
    finalPrompt,
    warnValid,
    hasSnapshot,
    onField,
  }: {
    pre: number;
    fin: number;
    wakePrompt: string;
    finalPrompt: string;
    warnValid: boolean;
    hasSnapshot: boolean;
    onField: (field: WarnField, value: string) => void;
  } = $props();
</script>

<section class="card preset-tonal-surface p-4 space-y-4">
  <div class="grid grid-cols-2 gap-4">
    <label class="text-sm space-y-1">
      <span class="opacity-70">{manage_schedules_pre_warn()}</span>
      <input
        class="input preset-tonal-surface w-full"
        type="number"
        min="1"
        value={pre}
        onchange={(e) => onField("pre", e.currentTarget.value)}
      />
    </label>
    <label class="text-sm space-y-1">
      <span class="opacity-70">{manage_schedules_final_warn()}</span>
      <input
        class="input preset-tonal-surface w-full"
        type="number"
        min="1"
        value={fin}
        onchange={(e) => onField("fin", e.currentTarget.value)}
      />
    </label>
  </div>
  {#if !warnValid}
    <p class="text-xs text-error-500 dark:text-error-400">
      {manage_schedules_warn_order()}
    </p>
  {:else if hasSnapshot}
    <p class="text-xs opacity-50">
      {manage_schedules_warnings({ pre, final: fin })}
    </p>
  {/if}

  <div class="space-y-1">
    <label class="text-sm opacity-70" for="wake-prompt">
      {manage_schedules_wake_prompt()}
    </label>
    <textarea
      id="wake-prompt"
      class="textarea preset-tonal-surface w-full"
      rows="3"
      value={wakePrompt}
      oninput={(e) => onField("wake", e.currentTarget.value)}></textarea>
    <p class="text-xs opacity-60">{manage_schedules_wake_hint()}</p>
  </div>

  <div class="space-y-1">
    <label class="text-sm opacity-70" for="final-warn-prompt">
      {manage_schedules_final_warn_prompt()}
    </label>
    <textarea
      id="final-warn-prompt"
      class="textarea preset-tonal-surface w-full"
      rows="3"
      value={finalPrompt}
      oninput={(e) => onField("final", e.currentTarget.value)}></textarea>
    <p class="text-xs opacity-60">
      {manage_schedules_final_warn_hint({ N: "{N}" })}
    </p>
  </div>
</section>
