<script lang="ts">
  // The schedules page IS the schedule: one tagma-wide work schedule
  // edited inline (no dialog, no list, no cron strings). The page keeps a
  // draft of the whole schedule, tracks dirtiness against the server
  // snapshot, and saves explicitly; the master switch is part of the same
  // draft. Times are stored UTC and edited in the operator's chosen clock
  // (a pure offset shift — across DST the local wall time drifts, noted
  // in the UI, nothing tz-ish is stored).
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import type { WorkSchedule, WorkScheduleSpec } from "@kallipai/kallip-client";
  import {
    DAY_MINUTES,
    formatClock,
    formatDay,
    hhmmToMinute,
    localOffsetMinutes,
    minuteToHHMM,
    offsetLabel,
    shiftMinute,
    validateSpec,
    windowStatus,
  } from "../../lib/manage/workSchedule.ts";
  import { untrack } from "svelte";
  import {
    common_save,
    manage_profiles_discard,
    manage_schedules_anchor_note,
    manage_schedules_clock_local,
    manage_schedules_clock_utc,
    manage_schedules_dst_note,
    manage_schedules_end_time,
    manage_schedules_error_days_empty,
    manage_schedules_error_every_hours,
    manage_schedules_error_generic,
    manage_schedules_error_length_min,
    manage_schedules_final_warn,
    manage_schedules_heading,
    manage_schedules_interval_hours,
    manage_schedules_interval_length,
    manage_schedules_interval_minute,
    manage_schedules_monthly_days,
    manage_schedules_monthly_short_note,
    manage_schedules_mode_interval,
    manage_schedules_mode_monthly,
    manage_schedules_mode_weekly,
    manage_schedules_next_start,
    manage_schedules_overnight,
    manage_schedules_pre_warn,
    manage_schedules_presets,
    manage_schedules_preset_allday,
    manage_schedules_preset_early,
    manage_schedules_preset_night,
    manage_schedules_preset_weekdays,
    manage_schedules_start_time,
    manage_schedules_status_active,
    manage_schedules_status_inside,
    manage_schedules_status_outside,
    manage_schedules_status_paused,
    manage_schedules_team_desc,
    manage_schedules_title,
    manage_schedules_unsaved,
    manage_schedules_wake_hint,
    manage_schedules_wake_now,
    manage_schedules_wake_prompt,
    manage_schedules_warn_invalid,
    manage_schedules_warn_order,
    manage_schedules_warnings,
    manage_schedules_weekly_days,
    manage_schedules_zero_window,
    manage_schedules_dow_1,
    manage_schedules_dow_2,
    manage_schedules_dow_3,
    manage_schedules_dow_4,
    manage_schedules_dow_5,
    manage_schedules_dow_6,
    manage_schedules_dow_7,
  } from "../../paraglide/messages.js";

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  $effect(() => {
    untrack(() => {
      schedulesStore.refresh();
      agentsStore.refresh();
    });
  });

  // The root agent carries the schedule; wake-now is its duty override.
  const rootAgent = $derived(
    agentsStore.agents.find((a) => a.created_by === null),
  );
  const rootOffDuty = $derived(rootAgent?.duty === "offduty");

  const utcOffset = localOffsetMinutes();
  // Local wall clock by default; UTC is an opt-in diagnostic view. The
  // manual choice persists like the theme mode (LightSwitch precedent).
  const UTC_PREF_KEY = "kallip:schedules-clock";
  let utc = $state(readClockPref());
  function setClock(next: boolean): void {
    utc = next;
    try {
      localStorage.setItem(UTC_PREF_KEY, next ? "utc" : "local");
    } catch {
      // Storage blocked; the choice lasts for this visit only.
    }
  }

  function readClockPref(): boolean {
    try {
      return localStorage.getItem(UTC_PREF_KEY) === "utc";
    } catch {
      // Storage blocked (private mode); default to the local clock.
      return false;
    }
  }

  // The draft is fully mutable (deep Writable) — it is the editing state,
  type MutableSpec =
    | { mode: "weekly"; days: number; start_minute: number; end_minute: number }
    | {
        mode: "monthly";
        days: number;
        start_minute: number;
        end_minute: number;
      }
    | {
        mode: "interval";
        every_hours: number;
        length_min: number;
        anchor: string;
      };
  // (structurally identical to the wire WorkScheduleSpec, minus readonly,
  // so the draft can be edited; assigning into PutWorkScheduleRequest is
  // still sound because the shapes are the same)
  type Draft = {
    spec: MutableSpec;
    pre_warn_minutes: number;
    final_warn_minutes: number;
    wake_prompt: string;
    status: "active" | "paused";
  };

  const defaultDraft = (): Draft => ({
    spec: {
      mode: "weekly",
      days: 0b0011_1111,
      start_minute: 540,
      end_minute: 1020,
    },
    pre_warn_minutes: 10,
    final_warn_minutes: 5,
    wake_prompt: "",
    status: "active",
  });

  let draft = $state<Draft | null>(null);
  $effect(() => {
    if (schedulesStore.hasLoaded && untrack(() => draft) === null) {
      const s = schedulesStore.schedule;
      draft = s
        ? {
            // $state.snapshot: the deep copy sanctioned at reactive
            // boundaries — structuredClone throws on $state proxies.
            spec: $state.snapshot(s.spec),
            pre_warn_minutes: s.pre_warn_minutes,
            final_warn_minutes: s.final_warn_minutes,
            wake_prompt: s.wake_prompt,
            status: s.status,
          }
        : defaultDraft();
    }
  });

  // --- clock helpers: display minute-of-day in the chosen clock ---

  const off = $derived(utc ? 0 : utcOffset);
  function showMinute(minute: number): string {
    return minuteToHHMM(shiftMinute(minute, off));
  }
  function editMinute(current: number, text: string): number | null {
    const m = hhmmToMinute(text);
    if (m === null) return null;
    return shiftMinute(m, -off);
  }

  // --- dirty tracking: field-level diff against the server snapshot ---

  const specEq = (a: WorkScheduleSpec, b: WorkScheduleSpec): boolean =>
    JSON.stringify(a) === JSON.stringify(b);
  const snapshot = $derived(schedulesStore.schedule);
  const dirty = $derived.by(() => {
    if (!draft || !schedulesStore.hasLoaded) return false;
    if (!snapshot) return true; // unsaved first draft
    return (
      !specEq(draft.spec, snapshot.spec) ||
      draft.pre_warn_minutes !== snapshot.pre_warn_minutes ||
      draft.final_warn_minutes !== snapshot.final_warn_minutes ||
      draft.wake_prompt !== snapshot.wake_prompt ||
      draft.status !== snapshot.status
    );
  });

  const specError = $derived(draft ? validateSpec(draft.spec) : null);
  const warnMinutesValid = $derived.by(() => {
    if (!draft) return false;
    const pre = draft.pre_warn_minutes;
    const fin = draft.final_warn_minutes;
    return (
      Number.isInteger(pre) &&
      Number.isInteger(fin) &&
      pre > 0 &&
      fin > 0 &&
      pre >= fin
    );
  });
  const canSave = $derived(
    dirty && !specError && warnMinutesValid && !schedulesStore.isSaving,
  );

  function discard(): void {
    const s = schedulesStore.schedule;
    draft = s
      ? {
          // Same proxy boundary as loadDraft; snapshot, never clone.
          spec: $state.snapshot(s.spec),
          pre_warn_minutes: s.pre_warn_minutes,
          final_warn_minutes: s.final_warn_minutes,
          wake_prompt: s.wake_prompt,
          status: s.status,
        }
      : defaultDraft();
  }

  // A fresh anchor timestamp, seconds zeroed so the minute stays the
  // only sub-hour signal. Built from the instant, never by regex surgery
  // on toISOString() — that once produced the invalid "16:26:00:00Z".
  function freshAnchor(): string {
    const d = new Date();
    d.setUTCSeconds(0, 0);
    return d.toISOString();
  }

  async function save(): Promise<void> {
    if (!draft || !canSave) return;
    // The draft goes out verbatim. For interval specs the anchor carries
    // the user's M (the effect keeps it in sync); the server either echoes
    // it unchanged or re-anchors at save time preserving that minute —
    // the client never rewrites the anchor itself.
    const spec = $state.snapshot(draft.spec);
    try {
      await schedulesStore.save({
        spec,
        pre_warn_minutes: draft.pre_warn_minutes,
        final_warn_minutes: draft.final_warn_minutes,
        wake_prompt: draft.wake_prompt,
        status: draft.status,
      });
    } catch {
      // surfaced via store error
    }
  }

  // --- weekly/monthly editors ---

  const WEEKDAYS = [1, 2, 3, 4, 5, 6, 7];
  function toggleWeekday(iso: number): void {
    if (!draft || draft.spec.mode !== "weekly") return;
    draft.spec = {
      ...draft.spec,
      days: draft.spec.days ^ (1 << (iso - 1)),
    };
  }
  function toggleMonthDay(day: number): void {
    if (!draft || draft.spec.mode !== "monthly") return;
    draft.spec = {
      ...draft.spec,
      days: draft.spec.days ^ (1 << (day - 1)),
    };
  }
  function setMode(mode: WorkScheduleSpec["mode"]): void {
    if (!draft) return;
    if (draft.spec.mode === mode) return;
    draft.spec =
      mode === "weekly"
        ? {
            mode: "weekly",
            days: 0b0011_1111,
            start_minute: 540,
            end_minute: 1020,
          }
        : mode === "monthly"
          ? {
              mode: "monthly",
              days: 1 << 0,
              start_minute: 540,
              end_minute: 1020,
            }
          : {
              mode: "interval",
              every_hours: 4,
              length_min: 60,
              anchor: freshAnchor(),
            };
  }

  const overnight = $derived(
    !!draft &&
      (draft.spec.mode === "weekly" || draft.spec.mode === "monthly") &&
      draft.spec.end_minute <= draft.spec.start_minute,
  );

  const presets = [
    {
      id: "weekdays",
      label: () => manage_schedules_preset_weekdays(),
      apply: () => ({ days: 0b0011_1111, start_minute: 540, end_minute: 1020 }),
    },
    {
      id: "early",
      label: () => manage_schedules_preset_early(),
      apply: () => ({ days: 0b0011_1111, start_minute: 360, end_minute: 840 }),
    },
    {
      id: "night",
      label: () => manage_schedules_preset_night(),
      apply: () => ({
        days: 0b0011_1111,
        start_minute: 22 * 60,
        end_minute: 6 * 60,
      }),
    },
    {
      id: "allday",
      label: () => manage_schedules_preset_allday(),
      apply: () => ({
        days: 0b0111_1111,
        start_minute: 0,
        end_minute: DAY_MINUTES,
      }),
    },
  ];
  function applyPreset(
    apply: () => { days: number; start_minute: number; end_minute: number },
  ): void {
    if (!draft || draft.spec.mode !== "weekly") return;
    draft.spec = { ...draft.spec, ...apply() };
  }

  // --- status line: client-side preview (same evaluator as backend) ---

  const statusNow = $derived.by(() => {
    if (!snapshot || snapshot.status !== "active") return null;
    return windowStatus(snapshot.spec, new Date());
  });

  function clockTime(d: Date): string {
    return formatClock(d, utc);
  }
  function nextStartText(): string | null {
    const st = statusNow;
    if (!st) return null;
    const sameDay = formatDay(st.nextStart, utc) === formatDay(new Date(), utc);
    return sameDay
      ? clockTime(st.nextStart)
      : `${formatDay(st.nextStart, utc)} ${clockTime(st.nextStart)}`;
  }

  function wakeNow(): void {
    if (rootAgent) agentsStore.toggleDuty(rootAgent.id).catch(() => {});
  }

  // Narrowed setters for the day-mask time inputs (the template's
  // spread would distribute over the union and confuse the checker).
  function setStartMinute(text: string): void {
    const spec = draft?.spec;
    if (!spec || spec.mode === "interval") return;
    const m = editMinute(spec.start_minute, text);
    if (m !== null) draft!.spec = { ...spec, start_minute: m };
  }
  function setEndMinute(text: string): void {
    const spec = draft?.spec;
    if (!spec || spec.mode === "interval") return;
    const m = editMinute(spec.end_minute, text);
    if (m !== null) draft!.spec = { ...spec, end_minute: m };
  }

  function errorText(err: string | null): string {
    switch (err) {
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

  // The interval editor's M field: bound to the anchor's minute-of-hour.
  // Kept in sync by effect both ways (draft → field on load/replace,
  // field → anchor as the user types).
  let anchorMinute = $state(0);
  $effect(() => {
    if (draft?.spec.mode === "interval") {
      const a = new Date(draft.spec.anchor);
      if (!Number.isNaN(a.getTime()))
        untrack(() => (anchorMinute = a.getUTCMinutes()));
    }
  });
  $effect(() => {
    const m = anchorMinute;
    if (
      draft?.spec.mode !== "interval" ||
      !Number.isInteger(m) ||
      m < 0 ||
      m > 59
    )
      return;
    const spec = draft.spec;
    const a = new Date(spec.anchor);
    if (Number.isNaN(a.getTime()) || a.getUTCMinutes() === m) return;
    a.setUTCMinutes(m, 0, 0);
    draft.spec = { ...spec, anchor: a.toISOString().replace(/\.000Z$/, "Z") };
  });
</script>

<svelte:head><title>{manage_schedules_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div class="flex items-center justify-between">
      <h1 class="text-xl font-semibold">{manage_schedules_heading()}</h1>
      <div class="flex gap-2">
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => schedulesStore.refresh(true)}>⟳</button
        >
      </div>
    </div>

    <p class="text-sm opacity-70">{manage_schedules_team_desc()}</p>

    {#if schedulesStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {schedulesStore.error}
      </p>
    {/if}

    {#if schedulesStore.isLoading && !schedulesStore.hasLoaded}
      <p class="opacity-60 text-sm">…</p>
    {:else if draft}
      <!-- master switch + status line -->
      <section class="card preset-tonal-surface p-4 space-y-3">
        <div class="flex items-center justify-between gap-4">
          <div class="flex items-center gap-2">
            <span
              class="size-2 rounded-full {draft.status === 'active'
                ? 'bg-success-500'
                : 'bg-surface-400-600'}"
              aria-hidden="true"
            ></span>
            <span class="text-sm font-medium">
              {draft.status === "active"
                ? manage_schedules_status_active()
                : manage_schedules_status_paused()}
            </span>
          </div>
          <button
            role="switch"
            aria-checked={draft.status === "active"}
            class="btn btn-sm {draft.status === 'active'
              ? 'preset-filled-primary-500'
              : 'preset-outlined-surface-500'}"
            onclick={() =>
              (draft!.status =
                draft!.status === "active" ? "paused" : "active")}
          >
            {draft.status === "active"
              ? manage_schedules_status_active()
              : manage_schedules_status_paused()}
          </button>
        </div>
        {#if statusNow}
          <p class="text-xs opacity-70">
            {statusNow.inside
              ? manage_schedules_status_inside({
                  time: clockTime(statusNow.nextEnd),
                })
              : manage_schedules_status_outside({
                  time: nextStartText() ?? "",
                })}
          </p>
        {/if}
        {#if rootOffDuty}
          <button
            class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
            onclick={wakeNow}>{manage_schedules_wake_now()}</button
          >
        {/if}
      </section>

      <!-- clock switch -->
      <div class="flex items-center justify-end gap-2 text-xs">
        <span class="opacity-50">{manage_schedules_dst_note()}</span>
        <div class="flex gap-1">
          <button
            class="btn btn-sm {utc
              ? 'preset-filled-primary-500'
              : 'preset-tonal-surface'}"
            aria-pressed={utc}
            onclick={() => setClock(true)}
          >
            {manage_schedules_clock_utc()}
          </button>
          <button
            class="btn btn-sm {!utc
              ? 'preset-filled-primary-500'
              : 'preset-tonal-surface'}"
            aria-pressed={!utc}
            onclick={() => setClock(false)}
          >
            {manage_schedules_clock_local()}（{offsetLabel(utcOffset)}）
          </button>
        </div>
      </div>

      <!-- period editor -->
      <section class="card preset-tonal-surface p-4 space-y-4">
        <div class="flex gap-1" role="tablist">
          {#each [["weekly", manage_schedules_mode_weekly()], ["monthly", manage_schedules_mode_monthly()], ["interval", manage_schedules_mode_interval()]] as [mode, label] (mode)}
            <button
              role="tab"
              aria-selected={draft.spec.mode === mode}
              class="btn btn-sm {draft.spec.mode === mode
                ? 'preset-filled-primary-500'
                : 'preset-tonal-surface'}"
              onclick={() => setMode(mode as WorkScheduleSpec["mode"])}
            >
              {label}
            </button>
          {/each}
        </div>

        {#if draft.spec.mode === "weekly" || draft.spec.mode === "monthly"}
          <div class="space-y-2">
            <p class="text-xs opacity-60">
              {draft.spec.mode === "weekly"
                ? manage_schedules_weekly_days()
                : manage_schedules_monthly_days()}
            </p>
            {#if draft.spec.mode === "weekly"}
              <div class="flex flex-wrap gap-1">
                {#each WEEKDAYS as iso (iso)}
                  <button
                    class="chip {(draft.spec.days & (1 << (iso - 1))) !== 0
                      ? 'preset-filled-primary-500'
                      : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
                    aria-pressed={(draft.spec.days & (1 << (iso - 1))) !== 0}
                    onclick={() => toggleWeekday(iso)}
                  >
                    {iso === 1
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
                                : manage_schedules_dow_7()}
                  </button>
                {/each}
              </div>
            {:else}
              <div class="flex flex-wrap gap-1">
                {#each Array.from({ length: 31 }, (_, i) => i + 1) as day (day)}
                  <button
                    class="chip size-9 {(draft.spec.days & (1 << (day - 1))) !==
                    0
                      ? 'preset-filled-primary-500'
                      : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
                    aria-pressed={(draft.spec.days & (1 << (day - 1))) !== 0}
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

          <div class="grid grid-cols-2 gap-4">
            <label class="text-sm space-y-1">
              <span class="opacity-70">
                {manage_schedules_start_time({
                  clock: utc ? "UTC" : offsetLabel(utcOffset),
                })}
              </span>
              <input
                class="input preset-tonal-surface w-full"
                type="time"
                value={showMinute(draft.spec.start_minute)}
                onchange={(e) => setStartMinute(e.currentTarget.value)}
              />
            </label>
            <label class="text-sm space-y-1">
              <span class="opacity-70">
                {manage_schedules_end_time({
                  clock: utc ? "UTC" : offsetLabel(utcOffset),
                })}
              </span>
              <input
                class="input preset-tonal-surface w-full"
                type="time"
                value={showMinute(draft.spec.end_minute)}
                onchange={(e) => setEndMinute(e.currentTarget.value)}
              />
            </label>
          </div>
          {#if overnight}
            <p class="text-xs opacity-60">{manage_schedules_overnight()}</p>
          {/if}
          {#if specError === "zero_window"}
            <p class="text-xs text-error-500 dark:text-error-400">
              {manage_schedules_zero_window()}
            </p>
          {/if}

          {#if draft.spec.mode === "weekly"}
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
        {:else}
          <div class="grid grid-cols-3 gap-4">
            <label class="text-sm space-y-1">
              <span class="opacity-70">{manage_schedules_interval_hours()}</span
              >
              <input
                class="input preset-tonal-surface w-full"
                type="number"
                min="1"
                max="23"
                bind:value={draft.spec.every_hours}
              />
            </label>
            <label class="text-sm space-y-1">
              <span class="opacity-70"
                >{manage_schedules_interval_minute()}</span
              >
              <input
                class="input preset-tonal-surface w-full"
                type="number"
                min="0"
                max="59"
                bind:value={anchorMinute}
              />
            </label>
            <label class="text-sm space-y-1">
              <span class="opacity-70"
                >{manage_schedules_interval_length()}</span
              >
              <input
                class="input preset-tonal-surface w-full"
                type="number"
                min="1"
                bind:value={draft.spec.length_min}
              />
            </label>
          </div>
          <p class="text-xs opacity-60">{manage_schedules_anchor_note()}</p>
        {/if}
        {#if specError && specError !== "zero_window"}
          <p class="text-xs text-error-500 dark:text-error-400">
            {errorText(specError)}
          </p>
        {/if}
      </section>

      <!-- warnings + wake prompt -->
      <section class="card preset-tonal-surface p-4 space-y-4">
        <div class="grid grid-cols-2 gap-4">
          <label class="text-sm space-y-1">
            <span class="opacity-70">{manage_schedules_pre_warn()}</span>
            <input
              class="input preset-tonal-surface w-full"
              type="number"
              min="1"
              bind:value={draft.pre_warn_minutes}
            />
          </label>
          <label class="text-sm space-y-1">
            <span class="opacity-70">{manage_schedules_final_warn()}</span>
            <input
              class="input preset-tonal-surface w-full"
              type="number"
              min="1"
              bind:value={draft.final_warn_minutes}
            />
          </label>
        </div>
        {#if !warnMinutesValid}
          <p class="text-xs text-error-500 dark:text-error-400">
            {manage_schedules_warn_order()}
          </p>
        {:else if snapshot}
          <p class="text-xs opacity-50">
            {manage_schedules_warnings({
              pre: draft.pre_warn_minutes,
              final: draft.final_warn_minutes,
            })}
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
            placeholder={manage_schedules_wake_hint()}
            bind:value={draft.wake_prompt}></textarea>
        </div>
      </section>

      <!-- save bar -->
      {#if dirty}
        <div
          class="flex items-center gap-3 sticky bottom-0 py-2 bg-surface-100-900 border-t border-surface-200-800"
        >
          <span class="chip preset-tonal-surface text-xs"
            >{manage_schedules_unsaved()}</span
          >
          <div class="flex-1"></div>
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={schedulesStore.isSaving}
            onclick={discard}
          >
            {manage_profiles_discard()}
          </button>
          <button
            class="btn btn-sm preset-filled-primary-500"
            disabled={!canSave}
            onclick={save}
          >
            {common_save()}
          </button>
        </div>
      {/if}
    {/if}
  </div>
</div>
