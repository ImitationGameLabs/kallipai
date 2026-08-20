<script lang="ts">
  // Structured schedule editor: the plain lane (repeat preset + start/end
  // times) covers the `M H * * D` subset from scheduleCron.ts; the advanced
  // lane keeps raw cron editing for everything else (steps, dom/month
  // parts) and for legacy rows that live outside the subset. Editing a
  // schedule whose crons parse into the subset pre-fills the plain lane;
  // anything else opens in the advanced lane, so no stored shape is
  // unreachable. Field drafts reset on each open transition (plain latch,
  // no self-trigger — same latch as ProviderDialog/ParkingDialog).
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    cronHasFiveFields,
    validateWarnMinutes,
  } from "../lib/manage/compute.ts";
  import { compileSubset, parseSubset } from "../lib/manage/scheduleCron.ts";
  import type { WorkSchedule } from "@kallipai/kallip-client";
  import {
    common_cancel,
    common_save,
    common_edit,
    manage_schedules_dow_0,
    manage_schedules_dow_1,
    manage_schedules_dow_2,
    manage_schedules_dow_3,
    manage_schedules_dow_4,
    manage_schedules_dow_5,
    manage_schedules_dow_6,
    common_create,
    manage_schedules_advanced,
    manage_schedules_advanced_hint,
    manage_schedules_cron_error,
    manage_schedules_edit_title,
    manage_schedules_end_cron,
    manage_schedules_end_time,
    manage_schedules_final_warn,
    manage_schedules_freq,
    manage_schedules_freq_custom,
    manage_schedules_freq_daily,
    manage_schedules_freq_weekdays,
    manage_schedules_freq_weekend,
    manage_schedules_name,
    manage_schedules_new_desc,
    manage_schedules_new_title,
    manage_schedules_overnight,
    manage_schedules_pre_warn,
    manage_schedules_presets,
    manage_schedules_preset_allday,
    manage_schedules_preset_weekdays,
    manage_schedules_preset_weekend,
    manage_schedules_start_cron,
    manage_schedules_start_time,
    manage_schedules_team_desc,
    manage_schedules_wake_hint,
    manage_schedules_wake_prompt,
    manage_schedules_zero_window,
  } from "../paraglide/messages.js";

  let {
    open,
    schedule = null,
    busy = false,
    onSubmit,
    onCancel,
  }: {
    open: boolean;
    /** Edit seed; null = create mode (team defaults). */
    schedule?: WorkSchedule | null;
    busy?: boolean;
    onSubmit: (v: {
      name: string;
      start_cron: string;
      end_cron: string;
      pre_warn_minutes: number;
      final_warn_minutes: number;
      wake_prompt: string;
    }) => void;
    onCancel: () => void;
  } = $props();

  type Freq = "weekdays" | "weekend" | "daily" | "custom";

  // Chip labels for the custom-days picker, index = cron dow (0 = Sunday).
  const dowLabels = [
    manage_schedules_dow_0,
    manage_schedules_dow_1,
    manage_schedules_dow_2,
    manage_schedules_dow_3,
    manage_schedules_dow_4,
    manage_schedules_dow_5,
    manage_schedules_dow_6,
  ];
  const ALL_DOWS = [0, 1, 2, 3, 4, 5, 6];

  function timeOf(cron: string, fallback: string): string {
    const f = parseSubset(cron);
    if (!f) return fallback;
    return `${String(f.hour).padStart(2, "0")}:${String(f.minute).padStart(2, "0")}`;
  }

  function initialFreq(cron: string): Freq {
    const dows = parseSubset(cron)?.dows;
    if (!dows) return "weekdays";
    const key = dows.join(",");
    if (key === "1,2,3,4,5") return "weekdays";
    if (key === "0,6") return "weekend";
    if (dows.length === 7) return "daily";
    return "custom";
  }

  function dowsFor(freq: Freq, custom: number[]): number[] {
    if (freq === "weekdays") return [1, 2, 3, 4, 5];
    if (freq === "weekend") return [0, 6];
    if (freq === "daily") return ALL_DOWS;
    return custom;
  }

  let formName = $state("");
  let advanced = $state(false);
  let freq = $state<Freq>("weekdays");
  let customDows = $state<number[]>([1, 2, 3, 4, 5]);
  let startTime = $state("09:00");
  let endTime = $state("17:00");
  let rawStart = $state("");
  let rawEnd = $state("");
  let preWarnMin = $state(10);
  let finalWarnMin = $state(5);
  let prompt = $state("");

  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      const startCron = schedule?.start_cron ?? "0 9 * * 1-5";
      const endCron = schedule?.end_cron ?? "0 17 * * 1-5";
      formName = schedule?.name ?? "";
      // Open in the advanced lane when either cron is outside the subset.
      advanced =
        parseSubset(startCron) === null || parseSubset(endCron) === null;
      freq = initialFreq(startCron);
      customDows = parseSubset(startCron)?.dows ?? [1, 2, 3, 4, 5];
      startTime = timeOf(startCron, "09:00");
      endTime = timeOf(endCron, "17:00");
      rawStart = startCron;
      rawEnd = endCron;
      preWarnMin = schedule?.pre_warn_minutes ?? 10;
      finalWarnMin = schedule?.final_warn_minutes ?? 5;
      prompt = schedule?.wake_prompt ?? "";
    }
    lastOpen = open;
  });

  function applyPreset(p: "weekdays" | "weekend" | "allday") {
    if (p === "weekdays") {
      freq = "weekdays";
      startTime = "09:00";
      endTime = "17:00";
    } else if (p === "weekend") {
      freq = "weekend";
      startTime = "10:00";
      endTime = "18:00";
    } else {
      freq = "daily";
      startTime = "00:00";
      endTime = "23:59";
    }
  }

  function parseTime(t: string): { hour: number; minute: number } | null {
    const m = /^(\d{2}):(\d{2})$/.exec(t);
    if (!m) return null;
    const hour = Number(m[1]);
    const minute = Number(m[2]);
    if (hour > 23 || minute > 59) return null;
    return { hour, minute };
  }

  const startT = $derived(parseTime(startTime));
  const endT = $derived(parseTime(endTime));
  // Zero-width window: same time on the same days never opens (the engine
  // compares next_end < next_start strictly), so it is rejected up front.
  const zeroWindow = $derived(
    !advanced &&
      startT !== null &&
      endT !== null &&
      startT.hour === endT.hour &&
      startT.minute === endT.minute,
  );
  const overnight = $derived(
    !advanced &&
      startT !== null &&
      endT !== null &&
      (endT.hour < startT.hour ||
        (endT.hour === startT.hour && endT.minute < startT.minute)),
  );
  const rawError = $derived(
    advanced && (!cronHasFiveFields(rawStart) || !cronHasFiveFields(rawEnd)),
  );
  const warnError = $derived(validateWarnMinutes(preWarnMin, finalWarnMin));
  const invalid = $derived(
    !formName ||
      !prompt ||
      zeroWindow ||
      rawError ||
      warnError !== null ||
      (!advanced && (startT === null || endT === null)),
  );

  function submit() {
    if (invalid) return;
    const crons = advanced
      ? { start: rawStart.trim(), end: rawEnd.trim() }
      : {
          start: compileSubset({ ...startT!, dows: dowsFor(freq, customDows) }),
          end: compileSubset({ ...endT!, dows: dowsFor(freq, customDows) }),
        };
    onSubmit({
      name: formName,
      start_cron: crons.start,
      end_cron: crons.end,
      pre_warn_minutes: preWarnMin,
      final_warn_minutes: finalWarnMin,
      wake_prompt: prompt,
    });
  }
</script>

<Dialog
  {open}
  onOpenChange={(e) => {
    if (!e.open) onCancel();
  }}
>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 space-y-4 max-h-[90vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {schedule
            ? manage_schedules_edit_title()
            : manage_schedules_new_title()}
        </Dialog.Title>
        <Dialog.Description class="text-sm opacity-60">
          {schedule
            ? manage_schedules_team_desc()
            : manage_schedules_new_desc()}
        </Dialog.Description>

        <div class="space-y-3 text-sm">
          <label class="block">
            <span class="opacity-60 text-xs">{manage_schedules_name()}</span>
            <input class="input w-full" bind:value={formName} />
          </label>

          {#if advanced}
            <label class="block">
              <span class="opacity-60 text-xs font-mono"
                >{manage_schedules_start_cron()}</span
              >
              <input class="input w-full font-mono" bind:value={rawStart} />
            </label>
            <label class="block">
              <span class="opacity-60 text-xs font-mono"
                >{manage_schedules_end_cron()}</span
              >
              <input class="input w-full font-mono" bind:value={rawEnd} />
            </label>
          {:else}
            <fieldset class="block">
              <legend class="opacity-60 text-xs"
                >{manage_schedules_freq()}</legend
              >
              <div class="flex flex-wrap gap-2 mt-1">
                {#each [["weekdays", manage_schedules_freq_weekdays()], ["weekend", manage_schedules_freq_weekend()], ["daily", manage_schedules_freq_daily()], ["custom", manage_schedules_freq_custom()]] as [value, label] (value)}
                  <label
                    class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 {freq ===
                    value
                      ? 'preset-filled-primary-500'
                      : ''} cursor-pointer"
                  >
                    <input
                      type="radio"
                      class="sr-only"
                      name="freq"
                      {value}
                      bind:group={freq}
                    />
                    {label}
                  </label>
                {/each}
              </div>
              {#if freq === "custom"}
                <div class="flex flex-wrap gap-2 mt-2">
                  {#each [1, 2, 3, 4, 5, 6, 0] as d (d)}
                    <button
                      type="button"
                      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 {customDows.includes(
                        d,
                      )
                        ? 'preset-filled-primary-500'
                        : ''}"
                      aria-pressed={customDows.includes(d)}
                      onclick={() =>
                        (customDows = customDows.includes(d)
                          ? customDows.filter((x) => x !== d)
                          : [...customDows, d])}
                    >
                      {dowLabels[d]?.() ?? ""}
                    </button>
                  {/each}
                </div>
              {/if}
            </fieldset>

            <div class="grid grid-cols-2 gap-2">
              <label class="block">
                <span class="opacity-60 text-xs"
                  >{manage_schedules_start_time()}</span
                >
                <input
                  type="time"
                  class="input w-full"
                  bind:value={startTime}
                />
              </label>
              <label class="block">
                <span class="opacity-60 text-xs"
                  >{manage_schedules_end_time()}</span
                >
                <input type="time" class="input w-full" bind:value={endTime} />
              </label>
            </div>
            {#if overnight}<p class="text-xs opacity-60">
                {manage_schedules_overnight()}
              </p>{/if}
            {#if zeroWindow}
              <p class="text-error-500 dark:text-error-400 text-xs">
                {manage_schedules_zero_window()}
              </p>
            {/if}

            <div>
              <span class="opacity-60 text-xs"
                >{manage_schedules_presets()}</span
              >
              <div class="flex flex-wrap gap-2 mt-1">
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                  onclick={() => applyPreset("weekdays")}
                  >{manage_schedules_preset_weekdays()}</button
                >
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                  onclick={() => applyPreset("weekend")}
                  >{manage_schedules_preset_weekend()}</button
                >
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                  onclick={() => applyPreset("allday")}
                  >{manage_schedules_preset_allday()}</button
                >
              </div>
            </div>
          {/if}

          {#if rawError}
            <p class="text-error-500 dark:text-error-400 text-xs">
              {manage_schedules_cron_error()}
            </p>
          {/if}

          <div>
            <button
              type="button"
              class="text-xs opacity-60 underline underline-offset-2"
              onclick={() => (advanced = !advanced)}
            >
              {manage_schedules_advanced()}
            </button>
            <p class="text-xs opacity-50">{manage_schedules_advanced_hint()}</p>
          </div>

          <div class="grid grid-cols-2 gap-2">
            <label class="block">
              <span class="opacity-60 text-xs"
                >{manage_schedules_pre_warn()}</span
              >
              <input
                type="number"
                class="input w-full"
                bind:value={preWarnMin}
              />
            </label>
            <label class="block">
              <span class="opacity-60 text-xs"
                >{manage_schedules_final_warn()}</span
              >
              <input
                type="number"
                class="input w-full"
                bind:value={finalWarnMin}
              />
            </label>
          </div>
          {#if warnError}
            <p class="text-error-500 dark:text-error-400 text-xs col-span-2">
              {warnError}
            </p>
          {/if}

          <label class="block">
            <span class="opacity-60 text-xs"
              >{manage_schedules_wake_prompt()}</span
            >
            <textarea class="input w-full" rows="3" bind:value={prompt}
            ></textarea>
            <span class="opacity-50 text-xs"
              >{manage_schedules_wake_hint()}</span
            >
          </label>
        </div>

        <div class="flex gap-2">
          <button
            type="button"
            class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
            onclick={onCancel}>{common_cancel()}</button
          >
          <button
            type="button"
            class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
            disabled={invalid || busy}
            onclick={submit}
            >{schedule ? common_save() : common_create()}</button
          >
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
