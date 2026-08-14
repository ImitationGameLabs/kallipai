<script lang="ts">
  import { schedulesStore } from "../../lib/manage/schedules.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { cronHasFiveFields, validateWarnMinutes } from "../../lib/manage/compute.ts";
  import { untrack } from "svelte";

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
    cronHasFiveFields(formData.start_cron) && cronHasFiveFields(formData.end_cron)
      ? null
      : "Each cron field must have exactly 5 tokens"
  );
  const warnError = $derived(validateWarnMinutes(formData.pre_warn_minutes, formData.final_warn_minutes));

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
      formData = { name: "", agent_id: "", start_cron: "0 9 * * 1-5", end_cron: "0 17 * * 1-5", pre_warn_minutes: 10, final_warn_minutes: 5, wake_prompt: "", timezone: "" };
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

<svelte:head><title>KallipAI · schedules</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div class="flex items-center justify-between">
      <h1 class="text-xl font-semibold">Schedules</h1>
      <div class="flex gap-2">
        <button class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500" onclick={() => schedulesStore.refresh(true)}>⟳</button>
        <button class="btn btn-sm preset-filled-primary-500" onclick={() => (showCreate = true)}>+ New Schedule</button>
      </div>
    </div>

    {#if schedulesStore.error}
      <p class="text-error-500 text-sm">{schedulesStore.error}</p>
    {/if}

    {#if schedulesStore.isLoading && !schedulesStore.hasLoaded}
      <p class="opacity-60 text-sm">Loading…</p>
    {:else if schedulesStore.schedules.length === 0}
      <p class="opacity-60 text-sm">No schedules.</p>
    {/if}
    {#each schedulesStore.schedules as sched (sched.id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-center justify-between">
          <div class="flex items-center gap-2">
            <span class="size-2 rounded-full {sched.status === 'active' ? 'bg-success-500' : 'bg-surface-400'}" aria-hidden="true"></span>
            <span class="font-medium">{sched.name}</span>
          </div>
          <span class="text-xs px-2 py-0.5 rounded-full preset-tonal-surface capitalize">{sched.status}</span>
        </div>
        <div class="grid grid-cols-2 gap-2 text-xs opacity-70">
          <div>Agent: <span class="font-mono">{sched.agent_id}</span></div>
          <div>Start: <span class="font-mono">{sched.start_cron}</span></div>
          <div>End: <span class="font-mono">{sched.end_cron}</span></div>
          <div>Warnings: {sched.pre_warn_minutes}m / {sched.final_warn_minutes}m</div>
        </div>
        {#if sched.timezone}
          <div class="text-xs opacity-50">TZ: {sched.timezone}</div>
        {/if}
        <div class="flex gap-2 pt-1">
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={schedulesStore.isInFlight(sched.id)}
            onclick={() => schedulesStore.toggleStatus(sched.id).catch(() => {})}
          >{sched.status === "active" ? "Pause" : "Resume"}</button>
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
            onclick={() => (deleteTarget = sched.id)}
          >Delete</button>
        </div>
      </div>
    {/each}
  </div>
</div>

<!-- Create dialog -->
<Dialog open={showCreate} onOpenChange={(e) => { if (!e.open) showCreate = false; }}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content class="card preset-tonal-surface w-full max-w-md p-6 space-y-4 max-h-[90vh] overflow-y-auto">
        <Dialog.Title class="text-lg font-semibold">New Schedule</Dialog.Title>
        <Dialog.Description class="text-sm opacity-60">Configure a new work schedule for an agent.</Dialog.Description>
        {#if schedulesStore.error}
          <p class="text-error-500 text-xs">{schedulesStore.error}</p>
        {/if}
        <div class="space-y-3 text-sm">
          <label class="block">
            <span class="opacity-60 text-xs">Name</span>
            <input class="input w-full" bind:value={formData.name} />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">Agent</span>
            <select class="select w-full" bind:value={formData.agent_id}>
              <option value="">Select agent…</option>
              {#each agentsStore.agents as agent}
                <option value={agent.id}>{agent.id} ({agent.role})</option>
              {/each}
            </select>
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">Start cron</span>
            <input class="input w-full font-mono" bind:value={formData.start_cron} />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">End cron</span>
            <input class="input w-full font-mono" bind:value={formData.end_cron} />
          </label>
        {#if cronError}<p class="text-error-500 text-xs">{cronError}</p>{/if}
          <div class="grid grid-cols-2 gap-2">
            <label class="block">
              <span class="opacity-60 text-xs">Pre-warn (min)</span>
              <input type="number" class="input w-full" bind:value={formData.pre_warn_minutes} />
            </label>
            <label class="block">
              <span class="opacity-60 text-xs">Final-warn (min)</span>
              <input type="number" class="input w-full" bind:value={formData.final_warn_minutes} />
            </label>
          </div>
          {#if warnError}<p class="text-error-500 text-xs col-span-2">{warnError}</p>{/if}
          <label class="block">
            <span class="opacity-60 text-xs">Wake prompt</span>
            <input class="input w-full" bind:value={formData.wake_prompt} />
          </label>
          <label class="block">
            <span class="opacity-60 text-xs">Timezone (optional)</span>
            <input class="input w-full" placeholder="America/New_York" bind:value={formData.timezone} />
          </label>
        </div>
        <div class="flex justify-end gap-2">
          <button class="btn preset-outlined-surface-500 hover:preset-filled-surface-500" onclick={() => (showCreate = false)}>Cancel</button>
          <button
            class="btn preset-filled-primary-500"
            disabled={!formData.name || !formData.agent_id || !formData.wake_prompt || schedulesStore.isCreating}
            onclick={onCreate}
          >Create</button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>

<ConfirmDialog
  busy={deleteTarget !== null && schedulesStore.isInFlight(deleteTarget)}
  open={deleteTarget !== null}
  title="Delete Schedule"
  description="This will permanently delete the work schedule. This cannot be undone."
  confirmLabel="Delete"
  tone="danger"
  onConfirm={onConfirmDelete}
  onCancel={() => (deleteTarget = null)}
/>
