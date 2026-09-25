<script lang="ts">
  // The schedules page IS the schedule: one tagma-wide work schedule
  // edited inline (no dialog, no list, no cron strings). The page keeps a
  // draft of the whole schedule, tracks dirtiness against the server
  // snapshot, and saves explicitly; the master switch is part of the same
  // draft. The wire spec is UTC; the draft lives in the operator's chosen
  // display clock, and the toFrame/fromFrame pair at the load/save
  // boundary is the only crossing (monthly stays in the UTC frame; 24/7
  // is the always variant).
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import type { WorkScheduleSpec } from "@kallipai/kallipai-client";
  import {
    formatClock,
    formatDay,
    fromFrame,
    localOffsetMinutes,
    offsetLabel,
    validateSpec,
    windowStatus,
  } from "../../lib/manage/workSchedule.ts";
  import SchedulePeriodEditor from "../../components/manage/SchedulePeriodEditor.svelte";
  import ScheduleWarnCard from "../../components/manage/ScheduleWarnCard.svelte";
  import type { WarnField } from "../../components/manage/ScheduleWarnCard.svelte";
  import ScheduleSaveBar from "../../components/manage/ScheduleSaveBar.svelte";
  import {
    applyFrame,
    canSave as canSaveImpl,
    defaultDraft,
    draftFrom,
    isDirty,
    reframe,
    reaches,
    warnMinutesValid as warnMinutesValidImpl,
  } from "../../lib/manage/scheduleDraft.ts";
  import type { Draft } from "../../lib/manage/scheduleDraft.ts";
  import { untrack } from "svelte";
  import {
    manage_schedules_clock_local,
    manage_schedules_clock_utc,
    manage_schedules_dst_note,
    manage_schedules_heading,
    manage_schedules_monthly_utc_note,
    manage_schedules_status_active,
    manage_schedules_status_always,
    manage_schedules_status_inside,
    manage_schedules_status_outside,
    manage_schedules_status_paused,
    manage_schedules_switch_blocked,
    manage_schedules_team_desc,
    manage_schedules_title,
    manage_schedules_wake_now,
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
  const UTC_PREF_KEY = "kallipai:schedules-clock";
  let utc = $state(readClockPref());
  function setClock(next: boolean): void {
    if (next === utc) return;
    // In-place reframe: the draft must survive the frame change
    // exactly, or the switch is refused (the guard renders why).
    if (draft !== null) {
      const reframed = reframe(
        draft.spec,
        utc ? 0 : utcOffset,
        next ? 0 : utcOffset,
      );
      if (reframed === null) return;
      draft.spec = reframed;
    }
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

  let draft = $state<Draft | null>(null);

  // The display frame: monthly keeps UTC (month-day masks cannot cross
  // month borders losslessly), otherwise the chosen clock.
  const effUtc = $derived(
    draft === null || draft.spec.mode !== "monthly" ? utc : true,
  );
  const effOff = $derived(effUtc ? 0 : utcOffset);

  $effect(() => {
    if (schedulesStore.hasLoaded && untrack(() => draft) === null) {
      const s = schedulesStore.schedule;
      const r = s
        ? draftFrom(s, effOff)
        : { draft: defaultDraft(), fellBack: false };
      if (r.fellBack) utc = true;
      draft = r.draft;
    }
  });

  // --- dirty tracking: field-level diff against the server snapshot ---

  const snapshot = $derived(schedulesStore.schedule);
  const dirty = $derived(
    isDirty(draft, snapshot, schedulesStore.hasLoaded, effOff),
  );

  const specError = $derived(draft ? validateSpec(draft.spec) : null);
  const warnMinutesValid = $derived(warnMinutesValidImpl(draft));
  // The UTC spec the draft converts to; null while it has no exact
  // equivalent (a full-day window on selected days outside the UTC
  // clock) — saving and leaving the frame are both blocked then.
  const wireSpec = $derived(
    draft === null ? null : fromFrame(draft.spec, effOff),
  );

  const canSave = $derived(
    canSaveImpl({
      dirty,
      specError,
      warnValid: warnMinutesValid,
      isSaving: schedulesStore.isSaving,
      wire: wireSpec,
    }),
  );

  // Clock-switch reachability: the draft must survive the frame change.
  const canShowUtc = $derived(reaches(draft?.spec ?? null, effOff, 0));
  const canShowLocal = $derived(
    reaches(draft?.spec ?? null, effOff, utcOffset),
  );

  function discard(): void {
    const s = schedulesStore.schedule;
    const r = s
      ? draftFrom(s, effOff)
      : { draft: defaultDraft(), fellBack: false };
    if (r.fellBack) utc = true;
    draft = r.draft;
  }

  async function save(): Promise<void> {
    if (!draft || !canSave) return;
    // The frame draft crosses to UTC exactly once, here — fromFrame is
    // the same conversion the dirty check uses, so what saves is what
    // the page showed. The response is the server's truth (an interval
    // re-anchors; a full-week full-day weekly normalizes to always) —
    // re-derive the draft from it so the editor shows what was stored
    // and dirty clears.
    const spec = wireSpec;
    if (spec === null) return;
    try {
      const saved = await schedulesStore.save({
        spec: $state.snapshot(spec),
        pre_warn_minutes: draft.pre_warn_minutes,
        final_warn_minutes: draft.final_warn_minutes,
        wake_prompt: draft.wake_prompt,
        final_warn_prompt: draft.final_warn_prompt,
        status: draft.status,
      });
      const framedSave = applyFrame($state.snapshot(saved.spec), effOff);
      if (framedSave.fellBack) utc = true;
      draft.spec = framedSave.spec;
      // The server trims and normalizes both custom prompts; mirror
      // them back or dirty stays stuck on whitespace-only differences.
      draft.final_warn_prompt = saved.final_warn_prompt ?? "";
      draft.wake_prompt = saved.wake_prompt;
    } catch {
      // surfaced via store error
    }
  }

  // One exit for the warn card's field edits: the card reports the
  // field key and raw string, the page owns the draft.
  function onWarnField(f: WarnField, v: string): void {
    if (!draft) return;
    if (f === "pre" || f === "fin") {
      const n = Number(v);
      if (Number.isInteger(n)) {
        if (f === "pre") draft.pre_warn_minutes = n;
        else draft.final_warn_minutes = n;
      }
    } else if (f === "wake") draft.wake_prompt = v;
    else draft.final_warn_prompt = v;
  }

  // --- status line: client-side preview (same evaluator as backend) ---

  const statusNow = $derived.by(() => {
    if (!snapshot || snapshot.status !== "active") return null;
    return windowStatus(snapshot.spec, new Date());
  });

  function clockTime(d: Date): string {
    return formatClock(d, effUtc);
  }
  function nextStartText(): string | null {
    const st = statusNow;
    if (!st) return null;
    const sameDay =
      formatDay(st.nextStart, effUtc) === formatDay(new Date(), effUtc);
    return sameDay
      ? clockTime(st.nextStart)
      : `${formatDay(st.nextStart, effUtc)} ${clockTime(st.nextStart)}`;
  }

  function wakeNow(): void {
    if (rootAgent) agentsStore.toggleDuty(rootAgent.id).catch(() => {});
  }
</script>

<svelte:head><title>{manage_schedules_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 md:p-6 max-w-2xl space-y-6">
    <div class="flex items-center justify-between">
      <!-- md+ keeps this h1; below md the shell top row carries the title (AppShell `title`). -->
      <h1 class="text-xl font-semibold hidden md:block">
        {manage_schedules_heading()}
      </h1>
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
              ? 'preset-filled-primary-200-800 border border-transparent'
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
        {#if snapshot && snapshot.spec.mode === "always" && snapshot.status === "active"}
          <p class="text-xs opacity-70">{manage_schedules_status_always()}</p>
        {:else if statusNow}
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
            class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-200-800"
            onclick={wakeNow}>{manage_schedules_wake_now()}</button
          >
        {/if}
      </section>

      <!-- clock switch -->
      <div
        class="flex flex-wrap md:flex-nowrap items-center justify-end gap-x-2 gap-y-1 text-xs"
      >
        <span class="opacity-50 basis-full md:basis-auto"
          >{manage_schedules_dst_note()}</span
        >
        <div class="flex gap-1 shrink-0">
          <button
            class="btn btn-sm {effUtc
              ? 'preset-filled-primary-200-800'
              : 'preset-tonal-surface'}"
            aria-pressed={effUtc}
            disabled={!canShowUtc || draft.spec.mode === "monthly"}
            onclick={() => setClock(true)}
          >
            {manage_schedules_clock_utc()}
          </button>
          <button
            class="btn btn-sm {!effUtc
              ? 'preset-filled-primary-200-800'
              : 'preset-tonal-surface'}"
            aria-pressed={!effUtc}
            disabled={!canShowLocal || draft.spec.mode === "monthly"}
            onclick={() => setClock(false)}
          >
            {manage_schedules_clock_local()}（{offsetLabel(utcOffset)}）
          </button>
        </div>
      </div>
      {#if draft.spec.mode === "monthly"}
        <p class="text-xs opacity-50 text-right">
          {manage_schedules_monthly_utc_note()}
        </p>
      {:else if !canShowUtc || !canShowLocal}
        <p class="text-xs opacity-50 text-right">
          {manage_schedules_switch_blocked()}
        </p>
      {/if}

      <!-- period editor -->
      <SchedulePeriodEditor
        spec={draft.spec}
        {effUtc}
        {utcOffset}
        onSpec={(next) => draft && (draft.spec = next)}
      />

      <!-- warnings + wake prompt -->
      <!-- always has no shift boundaries, so the warn/wake config below
           would be dead settings; hide the whole group in that mode -->
      {#if draft.spec.mode !== "always"}
        <ScheduleWarnCard
          pre={draft.pre_warn_minutes}
          fin={draft.final_warn_minutes}
          wakePrompt={draft.wake_prompt}
          finalPrompt={draft.final_warn_prompt}
          warnValid={warnMinutesValid}
          hasSnapshot={snapshot !== null}
          onField={onWarnField}
        />
      {/if}

      <!-- save bar -->
      {#if dirty}
        <ScheduleSaveBar
          isSaving={schedulesStore.isSaving}
          representable={wireSpec !== null}
          {canSave}
          onDiscard={discard}
          onSave={save}
        />
      {/if}
    {/if}
  </div>
</div>
