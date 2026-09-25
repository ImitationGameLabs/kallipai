<script lang="ts">
  // The period editor card: mode tabs, day masks, window rows, weekly
  // presets, and the interval fields. Owns the editing semantics and the
  // anchor-minute sync effects; every mutation leaves as one onSpec call
  // with the next spec value, so the draft (the state owner) stays in
  // the page. The page passes the effective clock frame for labels.
  import { TriangleAlert } from "@lucide/svelte";
  import { untrack } from "svelte";
  import type { WorkScheduleSpec } from "@kallipai/kallipai-client";
  import {
    hhmmToMinute,
    MAX_WINDOWS,
    minuteToHHMM,
    offsetLabel,
    validateSpec,
  } from "../../lib/manage/workSchedule.ts";
  import type { MutableSpec } from "../../lib/manage/scheduleDraft.ts";
  import {
    manage_schedules_add_window,
    manage_schedules_anchor_note,
    manage_schedules_always_note,
    manage_schedules_dow_1,
    manage_schedules_dow_2,
    manage_schedules_dow_3,
    manage_schedules_dow_4,
    manage_schedules_dow_5,
    manage_schedules_dow_6,
    manage_schedules_dow_7,
    manage_schedules_end_time,
    manage_schedules_error_days_empty,
    manage_schedules_error_every_hours,
    manage_schedules_error_generic,
    manage_schedules_error_length_min,
    manage_schedules_error_overlap,
    manage_schedules_error_windows_empty,
    manage_schedules_interval_hours,
    manage_schedules_interval_length,
    manage_schedules_interval_minute,
    manage_schedules_mode_interval,
    manage_schedules_mode_monthly,
    manage_schedules_mode_weekly,
    manage_schedules_monthly_days,
    manage_schedules_monthly_short_note,
    manage_schedules_overnight,
    manage_schedules_presets,
    manage_schedules_preset_always,
    manage_schedules_preset_early,
    manage_schedules_preset_night,
    manage_schedules_preset_weekdays,
    manage_schedules_remove_window,
    manage_schedules_start_time,
    manage_schedules_weekly_days,
    manage_schedules_windows_cap,
    manage_schedules_windows_label,
    manage_schedules_zero_window,
  } from "../../paraglide/messages.js";

  let {
    spec,
    effUtc,
    utcOffset,
    onSpec,
  }: {
    spec: MutableSpec;
    effUtc: boolean;
    utcOffset: number;
    onSpec: (next: MutableSpec) => void;
  } = $props();

  const WEEKDAYS = [1, 2, 3, 4, 5, 6, 7];
  function toggleWeekday(iso: number): void {
    if (spec.mode !== "weekly") return;
    onSpec({ ...spec, days: spec.days ^ (1 << (iso - 1)) });
  }
  function toggleMonthDay(day: number): void {
    if (spec.mode !== "monthly") return;
    onSpec({ ...spec, days: spec.days ^ (1 << (day - 1)) });
  }
  function dowLabel(iso: number): string {
    return iso === 1
      ? manage_schedules_dow_1()
      : iso === 2
        ? manage_schedules_dow_2()
        : iso === 3
          ? manage_schedules_dow_3()
          : iso === 4
            ? manage_schedules_dow_4()
            : iso === 5
              ? manage_schedules_dow_5()
              : iso === 6
                ? manage_schedules_dow_6()
                : manage_schedules_dow_7();
  }

  // A fresh anchor timestamp, seconds zeroed so the minute stays the
  // only sub-hour signal. Built from the instant, never by regex surgery
  // on toISOString() — that once produced the invalid "16:26:00:00Z".
  function freshAnchor(): string {
    const d = new Date();
    d.setUTCSeconds(0, 0);
    return d.toISOString();
  }
  function setMode(mode: WorkScheduleSpec["mode"]): void {
    if (spec.mode === mode) return;
    onSpec(
      mode === "weekly"
        ? {
            mode: "weekly",
            // all seven days: the agent has no weekend (the explicit
            // "weekdays" preset below covers the Mon-Fri intent)
            days: 0b0111_1111,
            windows: [{ start_minute: 540, end_minute: 1020 }],
          }
        : mode === "monthly"
          ? {
              mode: "monthly",
              days: 1 << 0,
              windows: [{ start_minute: 540, end_minute: 1020 }],
            }
          : mode === "interval"
            ? {
                mode: "interval",
                every_hours: 4,
                length_min: 60,
                anchor: freshAnchor(),
              }
            : { mode: "always" },
    );
  }

  const overnight = $derived(
    (spec.mode === "weekly" || spec.mode === "monthly") &&
      spec.windows.some((w) => w.end_minute <= w.start_minute),
  );

  type PresetWindow = { start_minute: number; end_minute: number };
  const presets = [
    {
      id: "weekdays",
      label: () => manage_schedules_preset_weekdays(),
      apply: () => ({
        days: 0b0001_1111,
        windows: [{ start_minute: 540, end_minute: 1020 }] as PresetWindow[],
      }),
    },
    {
      id: "early",
      label: () => manage_schedules_preset_early(),
      apply: () => ({
        days: 0b0001_1111,
        windows: [{ start_minute: 360, end_minute: 840 }] as PresetWindow[],
      }),
    },
    {
      id: "night",
      label: () => manage_schedules_preset_night(),
      apply: () => ({
        days: 0b0001_1111,
        windows: [
          { start_minute: 22 * 60, end_minute: 6 * 60 },
        ] as PresetWindow[],
      }),
    },
  ];
  function applyPreset(
    apply: () => { days: number; windows: PresetWindow[] },
  ): void {
    if (spec.mode !== "weekly") return;
    onSpec({ ...spec, ...apply() });
  }

  // Narrowed setters for the per-window time inputs (the template's
  // spread would distribute over the union and confuse the checker).
  function setWindowStart(i: number, text: string): void {
    if (spec.mode === "interval" || spec.mode === "always") return;
    const m = hhmmToMinute(text);
    if (m !== null) {
      onSpec({
        ...spec,
        windows: spec.windows.map((w, j) =>
          j === i ? { ...w, start_minute: m } : w,
        ),
      });
    }
  }
  function setWindowEnd(i: number, text: string): void {
    if (spec.mode === "interval" || spec.mode === "always") return;
    const m = hhmmToMinute(text);
    if (m !== null) {
      onSpec({
        ...spec,
        windows: spec.windows.map((w, j) =>
          j === i ? { ...w, end_minute: m } : w,
        ),
      });
    }
  }
  function removeWindow(i: number): void {
    if (spec.mode === "interval" || spec.mode === "always") return;
    if (spec.windows.length <= 1) return;
    onSpec({ ...spec, windows: spec.windows.filter((_, j) => j !== i) });
  }
  function addWindow(): void {
    if (spec.mode === "interval" || spec.mode === "always") return;
    if (spec.windows.length >= MAX_WINDOWS) return;
    onSpec({
      ...spec,
      windows: [...spec.windows, { start_minute: 540, end_minute: 1020 }],
    });
  }

  function errorText(err: string | null): string {
    switch (err) {
      case "windows_empty":
        return manage_schedules_error_windows_empty();
      case "windows_cap":
        return manage_schedules_windows_cap();
      case "windows_overlap":
        return manage_schedules_error_overlap();
      case "days_empty":
        return manage_schedules_error_days_empty();
      case "every_hours_range":
        return manage_schedules_error_every_hours();
      case "length_min_range":
        return manage_schedules_error_length_min();
      default:
        return manage_schedules_error_generic();
    }
  }
  const specError = $derived(validateSpec(spec));

  function setEveryHours(v: string): void {
    if (spec.mode !== "interval") return;
    const n = Number(v);
    if (Number.isInteger(n)) onSpec({ ...spec, every_hours: n });
  }
  function setLengthMin(v: string): void {
    if (spec.mode !== "interval") return;
    const n = Number(v);
    if (Number.isInteger(n)) onSpec({ ...spec, length_min: n });
  }

  // The interval editor's M field: bound to the anchor's minute-of-hour.
  // Kept in sync by effect both ways (spec → field on load/replace,
  // field → anchor as the user types).
  let anchorMinute = $state(0);
  $effect(() => {
    if (spec.mode === "interval") {
      const a = new Date(spec.anchor);
      if (!Number.isNaN(a.getTime()))
        untrack(() => (anchorMinute = a.getUTCMinutes()));
    }
  });
  $effect(() => {
    const m = anchorMinute;
    if (spec.mode !== "interval" || !Number.isInteger(m) || m < 0 || m > 59)
      return;
    const a = new Date(spec.anchor);
    if (Number.isNaN(a.getTime()) || a.getUTCMinutes() === m) return;
    a.setUTCMinutes(m, 0, 0);
    onSpec({ ...spec, anchor: a.toISOString().replace(/\.000Z$/, "Z") });
  });
</script>

<section class="card preset-tonal-surface p-4 space-y-4">
  <div class="flex gap-1" role="tablist">
    {#each [["always", manage_schedules_preset_always()], ["interval", manage_schedules_mode_interval()], ["weekly", manage_schedules_mode_weekly()], ["monthly", manage_schedules_mode_monthly()]] as [mode, label] (mode)}
      <button
        role="tab"
        aria-selected={spec.mode === mode}
        class="btn btn-sm {spec.mode === mode
          ? 'preset-filled-primary-200-800'
          : 'preset-tonal-surface'}"
        onclick={() => setMode(mode as WorkScheduleSpec["mode"])}
      >
        {label}
      </button>
    {/each}
  </div>

  {#if spec.mode === "weekly" || spec.mode === "monthly"}
    <div class="space-y-2">
      <p class="text-xs opacity-60">
        {spec.mode === "weekly"
          ? manage_schedules_weekly_days()
          : manage_schedules_monthly_days()}
      </p>
      {#if spec.mode === "weekly"}
        <div class="flex flex-wrap gap-1">
          {#each WEEKDAYS as iso (iso)}
            <button
              class="chip {(spec.days & (1 << (iso - 1))) !== 0
                ? 'preset-filled-primary-200-800 border border-transparent'
                : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
              aria-pressed={(spec.days & (1 << (iso - 1))) !== 0}
              onclick={() => toggleWeekday(iso)}
            >
              {dowLabel(iso)}
            </button>
          {/each}
        </div>
      {:else}
        <div class="grid mx-auto w-fit grid-cols-7 gap-1">
          {#each Array.from({ length: 31 }, (_, i) => i + 1) as day (day)}
            <button
              class="chip size-9 {(spec.days & (1 << (day - 1))) !== 0
                ? 'preset-filled-primary-200-800 border border-transparent'
                : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
              aria-pressed={(spec.days & (1 << (day - 1))) !== 0}
              onclick={() => toggleMonthDay(day)}
            >
              {day}
            </button>
          {/each}
        </div>
        <p class="text-xs opacity-50">
          {manage_schedules_monthly_short_note()}
        </p>
      {/if}
    </div>

    <div class="space-y-2">
      <p class="text-xs opacity-60">
        {manage_schedules_windows_label()}
      </p>
      {#each spec.windows as w, i (i)}
        <div class="flex items-end gap-2">
          <label class="text-sm space-y-1 flex-1">
            <span class="opacity-70">
              {manage_schedules_start_time({
                clock: effUtc ? "UTC" : offsetLabel(utcOffset),
              })}
            </span>
            <input
              class="input preset-tonal-surface w-full"
              type="time"
              value={minuteToHHMM(w.start_minute)}
              onchange={(e) => setWindowStart(i, e.currentTarget.value)}
            />
          </label>
          <label class="text-sm space-y-1 flex-1">
            <span class="opacity-70">
              {manage_schedules_end_time({
                clock: effUtc ? "UTC" : offsetLabel(utcOffset),
              })}
            </span>
            <input
              class="input preset-tonal-surface w-full"
              type="time"
              value={minuteToHHMM(w.end_minute)}
              onchange={(e) => setWindowEnd(i, e.currentTarget.value)}
            />
          </label>
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={spec.windows.length <= 1}
            aria-label={manage_schedules_remove_window()}
            title={manage_schedules_remove_window()}
            onclick={() => removeWindow(i)}
          >
            ×
          </button>
        </div>
      {/each}
      <button
        class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-200-800"
        disabled={spec.windows.length >= MAX_WINDOWS}
        onclick={addWindow}
      >
        + {manage_schedules_add_window()}
      </button>
    </div>
    {#if overnight}
      <p class="text-xs opacity-60">{manage_schedules_overnight()}</p>
    {/if}
    {#if specError === "zero_window"}
      <p class="text-xs text-error-500 dark:text-error-400">
        {manage_schedules_zero_window()}
      </p>
    {/if}

    {#if spec.mode === "weekly"}
      <div class="space-y-1">
        <p class="text-xs opacity-60">{manage_schedules_presets()}</p>
        <div class="flex flex-wrap gap-1">
          {#each presets as p (p.id)}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={() => applyPreset(p.apply)}
            >
              {p.label()}
            </button>
          {/each}
        </div>
      </div>
    {/if}
  {:else if spec.mode === "interval"}
    <div class="grid grid-cols-1 sm:grid-cols-3 gap-4">
      <label class="text-sm space-y-1">
        <span class="opacity-70">{manage_schedules_interval_hours()}</span>
        <input
          class="input preset-tonal-surface w-full"
          type="number"
          min="1"
          max="23"
          value={spec.every_hours}
          onchange={(e) => setEveryHours(e.currentTarget.value)}
        />
      </label>
      <label class="text-sm space-y-1">
        <span class="opacity-70">{manage_schedules_interval_minute()}</span>
        <input
          class="input preset-tonal-surface w-full"
          type="number"
          min="0"
          max="59"
          bind:value={anchorMinute}
        />
      </label>
      <label class="text-sm space-y-1">
        <span class="opacity-70">{manage_schedules_interval_length()}</span>
        <input
          class="input preset-tonal-surface w-full"
          type="number"
          min="1"
          value={spec.length_min}
          onchange={(e) => setLengthMin(e.currentTarget.value)}
        />
      </label>
    </div>
    <p class="text-xs opacity-60">{manage_schedules_anchor_note()}</p>
  {:else}
    <p class="text-xs opacity-60">{manage_schedules_always_note()}</p>
  {/if}
  {#if specError && specError !== "zero_window"}
    <p
      class="text-sm text-error-500 dark:text-error-400 flex items-center gap-1"
    >
      <TriangleAlert class="size-4.5 shrink-0" aria-hidden="true" />
      {errorText(specError)}
    </p>
  {/if}
</section>
