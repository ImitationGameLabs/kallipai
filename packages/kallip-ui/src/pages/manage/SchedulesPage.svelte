<script lang="ts">
  // Schedules page, tagma-wide semantics: one schedule sets the whole
  // team's work window (the root agent carries it and delegates on wake).
  // Cards show the plain-form summary (scheduleCron.describeCron) with a
  // raw-cron fallback for expressions outside the subset; legacy
  // per-agent rows are flagged instead of mistranslated. The wake-now
  // button is the single team duty override (page-level so it survives
  // the empty list); AgentsPage's per-row toggles are gone.
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import ScheduleForm from "../../components/ScheduleForm.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import { describeCron, nextStart } from "../../lib/manage/scheduleCron.ts";
  import { untrack } from "svelte";
  import {
    common_delete,
    common_edit,
    manage_schedules_active_exists,
    manage_schedules_empty,
    manage_schedules_heading,
    manage_schedules_legacy,
    manage_schedules_next_start,
    manage_schedules_new_schedule,
    manage_schedules_pause,
    manage_schedules_resume,
    manage_schedules_status_active,
    manage_schedules_status_paused,
    manage_schedules_title,
    manage_schedules_wake_now,
    manage_schedules_warnings,
    manage_schedules_delete_title,
    manage_schedules_delete_desc,
  } from "../../paraglide/messages.js";

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  $effect(() => {
    untrack(() => {
      schedulesStore.refresh();
      agentsStore.refresh();
    });
  });

  // The root agent is the schedule carrier: structural predicate
  // created_by === null, the same rule the backend create guard uses
  // (registry root_agent). Unresolved root (no roster yet) renders no
  // legacy flags rather than guessing.
  const rootAgent = $derived(
    agentsStore.agents.find((a) => a.created_by === null),
  );
  const rootOffDuty = $derived(rootAgent?.duty === "offduty");
  const activeTeamSchedule = $derived(
    schedulesStore.schedules.some(
      (s) => s.status === "active" && s.agent_id === rootAgent?.id,
    ),
  );

  // Dialog state: create (editing=null) or edit (editing=schedule).
  let formOpen = $state(false);
  let editing = $state<import("@kallipai/kallip-client").WorkSchedule | null>(
    null,
  );
  let deleteTarget = $state<string | null>(null);

  async function onSubmit(v: {
    name: string;
    start_cron: string;
    end_cron: string;
    pre_warn_minutes: number;
    final_warn_minutes: number;
    wake_prompt: string;
  }) {
    try {
      if (editing) {
        await schedulesStore.update(editing.id, v);
      } else {
        await schedulesStore.create({ ...v, agent_id: rootAgent?.id ?? "" });
      }
      formOpen = false;
      editing = null;
    } catch {
      // Error surfaced via store
    }
  }

  async function onConfirmDelete() {
    if (deleteTarget) {
      await schedulesStore.remove(deleteTarget).catch(() => {});
      deleteTarget = null;
    }
  }

  function wakeNow() {
    if (rootAgent) agentsStore.toggleDuty(rootAgent.id).catch(() => {});
  }

  function summary(s: { start_cron: string; end_cron: string }): string {
    return (
      describeCron(s.start_cron, s.end_cron) ??
      `${s.start_cron} → ${s.end_cron}`
    );
  }

  function nextStartText(s: { start_cron: string }): string | null {
    const next = nextStart(s.start_cron, new Date());
    if (!next) return null;
    return `${String(next.getUTCHours()).padStart(2, "0")}:${String(next.getUTCMinutes()).padStart(2, "0")}`;
  }
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
        <button
          class="btn btn-sm preset-filled-primary-500"
          onclick={() => {
            editing = null;
            formOpen = true;
          }}>{manage_schedules_new_schedule()}</button
        >
      </div>
    </div>

    {#if rootOffDuty}
      <button
        class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
        onclick={wakeNow}>{manage_schedules_wake_now()}</button
      >
    {/if}

    {#if schedulesStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {schedulesStore.error}
      </p>
    {/if}

    {#if activeTeamSchedule}
      <p class="text-xs opacity-60">{manage_schedules_active_exists()}</p>
    {/if}

    {#if schedulesStore.isLoading && !schedulesStore.hasLoaded}
      <p class="opacity-60 text-sm">…</p>
    {:else if schedulesStore.schedules.length === 0}
      <p class="opacity-60 text-sm">{manage_schedules_empty()}</p>
    {/if}

    {#each schedulesStore.schedules as sched (sched.id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-center justify-between">
          <div class="flex items-center gap-2">
            <span
              class="size-2 rounded-full {sched.status === 'active'
                ? 'bg-success-500'
                : 'bg-surface-400-600'}"
              aria-hidden="true"
            ></span>
            <span class="font-medium">{sched.name}</span>
            {#if rootAgent && sched.agent_id !== rootAgent.id}
              <span
                class="text-xs px-2 py-0.5 rounded-full preset-tonal-surface"
              >
                {manage_schedules_legacy()}
              </span>
            {/if}
          </div>
          <span
            class="text-xs px-2 py-0.5 rounded-full preset-tonal-surface capitalize"
            >{sched.status === "active"
              ? manage_schedules_status_active()
              : manage_schedules_status_paused()}</span
          >
        </div>
        <div class="text-xs opacity-70">
          {summary(sched)}
        </div>
        <div class="text-xs opacity-50">
          {manage_schedules_warnings({
            pre: sched.pre_warn_minutes,
            final: sched.final_warn_minutes,
          })}
          {#if sched.status === "active" && nextStartText(sched)}
            · {manage_schedules_next_start({ time: nextStartText(sched)! })}
          {/if}
        </div>
        <div class="flex gap-2 pt-1">
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={schedulesStore.isInFlight(sched.id)}
            onclick={() => {
              editing = sched;
              formOpen = true;
            }}>{common_edit()}</button
          >
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={schedulesStore.isInFlight(sched.id)}
            onclick={() =>
              schedulesStore.toggleStatus(sched.id).catch(() => {})}
            >{sched.status === "active"
              ? manage_schedules_pause()
              : manage_schedules_resume()}</button
          >
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
            onclick={() => (deleteTarget = sched.id)}>{common_delete()}</button
          >
        </div>
      </div>
    {/each}
  </div>
</div>

<ScheduleForm
  open={formOpen}
  schedule={editing}
  busy={schedulesStore.isCreating ||
    (editing !== null && schedulesStore.isInFlight(editing.id))}
  {onSubmit}
  onCancel={() => {
    formOpen = false;
    editing = null;
  }}
/>

<ConfirmDialog
  busy={deleteTarget !== null && schedulesStore.isInFlight(deleteTarget)}
  open={deleteTarget !== null}
  title={manage_schedules_delete_title()}
  description={manage_schedules_delete_desc()}
  confirmLabel={common_delete()}
  tone="danger"
  onConfirm={onConfirmDelete}
  onCancel={() => (deleteTarget = null)}
/>
