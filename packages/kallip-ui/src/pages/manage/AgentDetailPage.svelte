<script lang="ts">
  import type { AgentStatusResponse } from "@kallipai/kallip-client";
  import { managementBackend } from "../../lib/manage/client.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import { navigate } from "../../lib/shell/port.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import StateDot from "../../components/manage/StateDot.svelte";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import { getLocale } from "../../paraglide/runtime.js";

  import {
    common_save,
    common_remove,
    manage_agent_title,
    manage_agent_back,
    manage_agent_role,
    manage_agent_duty,
    manage_agent_duty_onduty,
    manage_agent_duty_offduty,
    agent_state_idle,
    agent_state_busy,
    agent_state_faulted,
    manage_agent_created_by,
    manage_agent_workspace,
    manage_agent_description,
    manage_agent_context_usage,
    manage_agent_cumulative_label,
    manage_agent_turns,
    manage_agent_pinned_items,
    manage_agent_turn_tokens,
    manage_agent_last_prompt,
    manage_agent_cumulative_in,
    manage_agent_cumulative_out,
    manage_agent_recent_retries,
    manage_agent_retry_line,
    manage_agent_retry_retried,
    manage_agent_retry_exhausted,
    manage_agent_toggle_duty,
    manage_agent_interrupt,
    manage_agent_remove_agent,
    manage_agent_remove_agent_desc_short,
  } from "../../paraglide/messages.js";
  let { id, basePath = "/local/manage" }: { id: string; basePath?: string } =
    $props();

  let status = $state<AgentStatusResponse | null>(null);
  let statusError = $state<string | null>(null);
  let isLoading = $state(false);
  let pollHandle: ReturnType<typeof setInterval> | null = null;

  let editingRole = $state(false);
  let editingDesc = $state(false);
  let roleDraft = $state("");
  let descDraft = $state("");
  let showRemoveDialog = $state(false);

  async function fetchStatus() {
    isLoading = true;
    statusError = null;
    try {
      status = await managementBackend().getAgentStatus(id);
    } catch (e) {
      statusError = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    fetchStatus();
    agentsStore.refresh();
    pollHandle = setInterval(fetchStatus, 5000);
    return () => {
      if (pollHandle) clearInterval(pollHandle);
    };
  });

  const agent = $derived(agentsStore.agents.find((a) => a.id === id));
  const cumulativeTokens = $derived(
    status
      ? status.context.cumulative_usage.prompt_tokens +
          status.context.cumulative_usage.completion_tokens
      : 0,
  );

  function fmtRetry(r: {
    timestamp: number;
    attempt: number;
    max_attempts: number;
    error: string;
  }): string {
    const date = new Date(r.timestamp * 1000).toLocaleString(getLocale());
    const outcome =
      r.attempt < r.max_attempts
        ? manage_agent_retry_retried()
        : manage_agent_retry_exhausted();
    return manage_agent_retry_line({ date, error: r.error, outcome });
  }

  async function onSaveRole() {
    await agentsStore.updateMetadata(id, { role: roleDraft }).catch(() => {});
    editingRole = false;
  }
  async function onSaveDesc() {
    await agentsStore
      .updateMetadata(id, { description: descDraft })
      .catch(() => {});
    editingDesc = false;
  }
  async function onConfirmRemove() {
    await agentsStore.remove(id).catch(() => {});
    showRemoveDialog = false;
    navigate("/local/manage/agents");
  }
</script>

<svelte:head><title>{manage_agent_title({ id })}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div class="flex items-center gap-3">
      <a
        href={`${basePath}/agents`}
        class="btn btn-sm preset-outlined-surface-500">{manage_agent_back()}</a
      >
      <h1 class="text-xl font-semibold font-mono truncate">{id}</h1>
    </div>

    {#if statusError}
      <p class="text-error-500 dark:text-error-400 text-sm">{statusError}</p>
    {/if}

    {#if agent}
      <section class="card preset-tonal-surface p-5 space-y-3">
        <div class="flex items-center gap-2">
          <StateDot state={agent.state} />
          <span class="font-medium"
            >{agent.state === "idle"
              ? agent_state_idle()
              : agent.state === "busy"
                ? agent_state_busy()
                : agent_state_faulted()}</span
          >
          {#if agent.activity}
            <span class="opacity-60 text-sm">· {agent.activity}</span>
          {/if}
        </div>
        <div class="grid grid-cols-2 gap-3 text-sm">
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_role()}</span
            >
            {#if editingRole}
              <div class="flex gap-1 mt-1">
                <input class="input flex-1" bind:value={roleDraft} />
                <button
                  class="btn btn-sm preset-filled-primary-500"
                  disabled={agentsStore.isInFlight(id)}
                  onclick={onSaveRole}>{common_save()}</button
                >
                <button
                  class="btn btn-sm preset-outlined-surface-500"
                  onclick={() => (editingRole = false)}>×</button
                >
              </div>
            {:else}
              <button
                type="button"
                class="cursor-pointer hover:opacity-80 text-left bg-transparent border-none p-0"
                onclick={() => {
                  roleDraft = agent.role;
                  editingRole = true;
                }}
              >
                {agent.role || "—"}
              </button>
            {/if}
          </div>
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_duty()}</span
            >
            <span
              >{agent.duty === "onduty"
                ? manage_agent_duty_onduty()
                : manage_agent_duty_offduty()}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_created_by()}</span
            >
            <span class="font-mono text-xs">{agent.created_by ?? "root"}</span>
          </div>
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_workspace()}</span
            >
            <span class="font-mono text-xs truncate block"
              >{agent.workspace_root}</span
            >
          </div>
        </div>
        <div>
          <span class="opacity-60 text-xs uppercase tracking-wide block"
            >{manage_agent_description()}</span
          >
          {#if editingDesc}
            <div class="flex gap-1 mt-1">
              <input class="input flex-1" bind:value={descDraft} />
              <button
                class="btn btn-sm preset-filled-primary-500"
                disabled={agentsStore.isInFlight(id)}
                onclick={onSaveDesc}>{common_save()}</button
              >
              <button
                class="btn btn-sm preset-outlined-surface-500"
                onclick={() => (editingDesc = false)}>×</button
              >
            </div>
          {:else}
            <button
              type="button"
              class="cursor-pointer hover:opacity-80 text-sm text-left bg-transparent border-none p-0"
              onclick={() => {
                descDraft = agent.description;
                editingDesc = true;
              }}
            >
              {agent.description || "—"}
            </button>
          {/if}
        </div>
      </section>
    {/if}

    {#if status}
      <section class="card preset-tonal-surface p-5 space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_agent_context_usage()}
        </h2>
        <BudgetBar
          consumed={cumulativeTokens}
          budget={status.token_budget}
          label={manage_agent_cumulative_label()}
        />
        <div class="grid grid-cols-2 gap-3 text-sm">
          <div>
            <span class="opacity-60 text-xs">{manage_agent_turns()}</span><span
              class="font-medium ml-2">{status.context.turn_count}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs">{manage_agent_pinned_items()}</span><span
              class="font-medium ml-2"
              >{status.context.pinned_items.length}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs">{manage_agent_turn_tokens()}</span><span
              class="font-medium ml-2"
              >{formatTokenCount(status.context.turn_tokens)}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs">{manage_agent_last_prompt()}</span><span
              class="font-medium ml-2"
              >{status.context.last_prompt_tokens
                ? formatTokenCount(status.context.last_prompt_tokens)
                : "—"}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs">{manage_agent_cumulative_in()}</span
            ><span class="font-medium ml-2"
              >{formatTokenCount(
                status.context.cumulative_usage.prompt_tokens,
              )}</span
            >
          </div>
          <div>
            <span class="opacity-60 text-xs">{manage_agent_cumulative_out()}</span
            ><span class="font-medium ml-2"
              >{formatTokenCount(
                status.context.cumulative_usage.completion_tokens,
              )}</span
            >
          </div>
        </div>
      </section>

      {#if status.recent_retries.length > 0}
        <section class="card preset-tonal-surface p-5 space-y-2">
          <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
            {manage_agent_recent_retries()}
          </h2>
          {#each status.recent_retries as r}
            <div class="text-xs opacity-70">{fmtRetry(r)}</div>
          {/each}
        </section>
      {/if}
    {/if}

    {#if agent}
      <section class="flex flex-wrap gap-2">
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
          >{manage_agent_toggle_duty()}</button
        >
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
          onclick={() => (showRemoveDialog = true)}
          >{manage_agent_remove_agent()}</button
        >
      </section>
    {/if}
  </div>
</div>

<ConfirmDialog
  busy={showRemoveDialog && agentsStore.isInFlight(id)}
  open={showRemoveDialog}
  title={manage_agent_remove_agent()}
  description={manage_agent_remove_agent_desc_short()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={onConfirmRemove}
  onCancel={() => (showRemoveDialog = false)}
/>
