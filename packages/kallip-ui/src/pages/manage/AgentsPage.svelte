<script lang="ts">
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { navigate } from "../../lib/shell/port.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  import StateDot from "../../components/manage/StateDot.svelte";
  import {
    common_loading,
    common_remove,
    manage_agents_title,
    manage_agents_heading,
    manage_agents_empty,
    manage_agent_duty_onduty,
    manage_agent_duty_offduty,
    manage_agents_details,
    manage_agent_interrupt,
    manage_agent_remove_agent,
    manage_agent_remove_agent_desc,
  } from "../../paraglide/messages.js";

  $effect(() => {
    agentsStore.startPolling(5000);
    return () => agentsStore.stopPolling();
  });

  // Remove confirmation state.
  let removeTarget = $state<string | null>(null);

  async function onConfirmRemove() {
    if (removeTarget) {
      await agentsStore.remove(removeTarget).catch(() => {});
      removeTarget = null;
    }
  }

  function dutyLabel(duty: "onduty" | "offduty"): string {
    return duty === "onduty"
      ? manage_agent_duty_onduty()
      : manage_agent_duty_offduty();
  }

  // Root agent (created_by is null only for the tagma-managed root) sorts
  // first; rootCount feeds the divider position in the template.
  const rootCount = $derived(
    agentsStore.agents.filter((a) => a.created_by === null).length,
  );
  const orderedAgents = $derived([
    ...agentsStore.agents.filter((a) => a.created_by === null),
    ...agentsStore.agents.filter((a) => a.created_by !== null),
  ]);
</script>

<svelte:head><title>{manage_agents_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div class="flex items-center justify-between">
      <h1 class="text-xl font-semibold">{manage_agents_heading()}</h1>
      <button
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        onclick={() => agentsStore.refresh(true)}>⟳</button
      >
    </div>

    {#if agentsStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {agentsStore.error}
      </p>
    {/if}

    {#if agentsStore.isLoading && !agentsStore.hasLoaded}
      <p class="opacity-60 text-sm">{common_loading()}</p>
    {:else if agentsStore.agents.length === 0}
      <p class="opacity-60 text-sm">{manage_agents_empty()}</p>
    {/if}
    <div class="space-y-2">
      {#each orderedAgents as agent, idx (agent.id)}
        {#if idx === rootCount && rootCount > 0}
          <hr class="border-surface-300-700" />
        {/if}
        <div class="card preset-tonal-surface p-4">
          <div class="flex items-start justify-between gap-3">
            <div class="flex items-center gap-2 min-w-0">
              <StateDot state={agent.state} />
              <div class="min-w-0">
                <div class="text-sm font-medium truncate">{agent.role || agent.id}</div>
                {#if agent.role}
                  <div class="font-mono text-xs opacity-60 truncate">{agent.id}</div>
                {/if}
              </div>
            </div>
            <div class="flex flex-col items-end gap-1">
              <span
                class="text-xs px-2 py-0.5 rounded-full preset-tonal-surface"
              >
                {dutyLabel(agent.duty)}
              </span>
              {#if agent.activity}
                <span class="text-xs opacity-50 truncate max-w-32"
                  >{agent.activity}</span
                >
              {/if}
            </div>
          </div>

          {#if agent.state === "faulted" && agent.faulted_reason}
            <p class="text-error-500 dark:text-error-400 text-xs mt-2">
              {agent.faulted_reason}
            </p>
          {/if}

          <div class="flex flex-wrap gap-2 mt-3">
            <a
              href={`${basePath}/agents/${agent.id}`}
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
              >{manage_agents_details()}</a
            >
            {#if agent.state === "busy"}
              <button
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={agentsStore.isInFlight(agent.id)}
                onclick={() => agentsStore.interrupt(agent.id).catch(() => {})}
                >{manage_agent_interrupt()}</button
              >
            {/if}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
              disabled={agentsStore.isInFlight(agent.id)}
              onclick={() => agentsStore.toggleDuty(agent.id).catch(() => {})}
              >{agent.duty === "onduty"
                ? manage_agent_duty_offduty()
                : manage_agent_duty_onduty()}</button
            >
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
              onclick={() => (removeTarget = agent.id)}
              >{common_remove()}</button
            >
          </div>
        </div>
      {/each}
    </div>
  </div>
</div>

<ConfirmDialog
  busy={removeTarget !== null && agentsStore.isInFlight(removeTarget)}
  open={removeTarget !== null}
  title={manage_agent_remove_agent()}
  description={manage_agent_remove_agent_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={onConfirmRemove}
  onCancel={() => (removeTarget = null)}
/>
