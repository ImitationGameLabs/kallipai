<script lang="ts">
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    cronHasFiveFields,
    validateWarnMinutes,
  } from "../../lib/manage/compute.ts";
  import { untrack } from "svelte";
  import {
    common_loading,
    common_cancel,
    common_delete,
    manage_schedules_title,
    manage_schedules_heading,
    manage_schedules_new_schedule,
    manage_schedules_empty,
    manage_schedules_agent,
    manage_schedules_start,
    manage_schedules_end,
    manage_schedules_warnings,
    manage_schedules_tz,
    manage_schedules_status_active,
    manage_schedules_status_paused,
    manage_schedules_pause,
    manage_schedules_resume,
    manage_schedules_new_title,
    manage_schedules_new_desc,
    manage_schedules_name,
    manage_schedules_agent_name,
    manage_schedules_select_agent,
    manage_schedules_start_cron,
    manage_schedules_end_cron,
    manage_schedules_pre_warn,
    manage_schedules_final_warn,
    manage_schedules_wake_prompt,
    manage_schedules_timezone,
    common_create,
    manage_schedules_cron_error,
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

  // Form state for create
  let showCreate = $state(false);
  let formData = $state({
    name: "",
    agent_id: "",
    start_cron: "0 9 * * 1-5",
    end_cron: "0 17 * * 1-5",
    pre_warn_minutes: 10,
    final_warn_minutes: 5,
    wake_prompt: "",
    timezone: "",
  });

  const cronError = $derived(
    cronHasFiveFields(formData.start_cron) &&
      cronHasFiveFields(formData.end_cron)
      ? null
      : manage_schedules_cron_error(),
  );
  const warnError = $derived(
    validateWarnMinutes(formData.pre_warn_minutes, formData.final_warn_minutes),
  );

  // Delete confirmation
  let deleteTarget = $state<string | null>(null);

  async function onCreate() {
    try {
      await schedulesStore.create({
        name: formData.name,
        agent_id: formData.agent_id,
        start_cron: formData.start_cron,
        end_cron: formData.end_cron,
        pre_warn_minutes: formData.pre_warn_minutes,
        final_warn_minutes: formData.final_warn_minutes,
        wake_prompt: formData.wake_prompt,
        timezone: formData.timezone || null,
      });
      showCreate = false;
      formData = {
        name: "",
        agent_id: "",
        start_cron: "0 9 * * 1-5",
        end_cron: "0 17 * * 1-5",
        pre_warn_minutes: 10,
        final_warn_minutes: 5,
        wake_prompt: "",
        timezone: "",
      };
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
          onclick={() => (showCreate = true)}
          >{manage_schedules_new_schedule()}</button
        >
      </div>
    </div>

    {#if schedulesStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {schedulesStore.error}
      </p>
    {/if}

    {#if schedulesStore.isLoading && !schedulesStore.hasLoaded}
      <p class="opacity-60 text-sm">{common_loading()}</p>
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
          </div>
          <span
            class="text-xs px-2 py-0.5 rounded-full preset-tonal-surface capitalize"
            >{sched.status === "active"
              ? manage_schedules_status_active()
              : manage_schedules_status_paused()}</span
          >
        </div>
        <div class="grid grid-cols-2 gap-2 text-xs opacity-70">
          <div>{manage_schedules_agent({ id: sched.agent_id })}</div>
          <div>{manage_schedules_start({ cron: sched.start_cron })}</div>
          <div>{manage_schedules_end({ cron: sched.end_cron })}</div>
          <div>
            {manage_schedules_warnings({
              pre: sched.pre_warn_minutes,
              final: sched.final_warn_minutes,
            })}
          </div>
        </div>
        {#if sched.timezone}
          <div class="text-xs opacity-50">
            {manage_schedules_tz({ tz: sched.timezone })}
          </div>
        {/if}
        <div class="flex gap-2 pt-1">
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

<!-- Create dialog -->
<Dialog
  open={showCreate}
  onOpenChange={(e) => {
    if (!e.open) showCreate = false;
  }}
>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 space-y-4 max-h-[90vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold"
          >{manage_schedules_new_title()}</Dialog.Title
        >
        <Dialog.Description class="text-sm opacity-60"
          >{manage_schedules_new_desc()}</Dialog.Description
        >
        {#if schedulesStore.error}
          <p class="text-error-500 dark:text-error-400 text-xs">
            {schedulesStore.error}
          </p>
        {/if}
        <div class="space-y-3 text-sm">
          <label class="block">
            <span class="opacity-60 text-xs">{manage_schedules_name()}</span>
            <input class="input w-full" bind:value={formData.name} />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs"
              >{manage_schedules_agent_name()}</span
            >
            <select class="select w-full" bind:value={formData.agent_id}>
              <option value="">{manage_schedules_select_agent()}</option>
              {#each agentsStore.agents as agent}
                <option value={agent.id}>{agent.id} ({agent.role})</option>
              {/each}
            </select>
          </label>
          <label class="block">
            <span class="opacity-60 text-xs"
              >{manage_schedules_start_cron()}</span
            >
            <input
              class="input w-full font-mono"
              bind:value={formData.start_cron}
            />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">{manage_schedules_end_cron()}</span
            >
            <input
              class="input w-full font-mono"
              bind:value={formData.end_cron}
            />
          </label>
          {#if cronError}<p class="text-error-500 dark:text-error-400 text-xs">
              {cronError}
            </p>{/if}
          <div class="grid grid-cols-2 gap-2">
            <label class="block">
              <span class="opacity-60 text-xs"
                >{manage_schedules_pre_warn()}</span
              >
              <input
                type="number"
                class="input w-full"
                bind:value={formData.pre_warn_minutes}
              />
            </label>
            <label class="block">
              <span class="opacity-60 text-xs"
                >{manage_schedules_final_warn()}</span
              >
              <input
                type="number"
                class="input w-full"
                bind:value={formData.final_warn_minutes}
              />
            </label>
          </div>
          {#if warnError}<p
              class="text-error-500 dark:text-error-400 text-xs col-span-2"
            >
              {warnError}
            </p>{/if}
          <label class="block">
            <span class="opacity-60 text-xs"
              >{manage_schedules_wake_prompt()}</span
            >
            <input class="input w-full" bind:value={formData.wake_prompt} />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">{manage_schedules_timezone()}</span
            >
            <input
              class="input w-full"
              placeholder="America/New_York"
              bind:value={formData.timezone}
            />
          </label>
        </div>
        <div class="flex gap-2">
          <button
            class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
            onclick={() => (showCreate = false)}>{common_cancel()}</button
          >
          <button
            class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
            disabled={!formData.name ||
              !formData.agent_id ||
              !formData.wake_prompt ||
              schedulesStore.isCreating}
            onclick={onCreate}>{common_create()}</button
          >
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>

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
