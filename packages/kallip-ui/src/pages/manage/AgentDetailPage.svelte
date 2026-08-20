<script lang="ts">
  import type { AgentStatusResponse } from "@kallipai/kallip-client";
  import { CalendarClock, Clock } from "@lucide/svelte";
  import { MoreVertical, Pencil, Trash } from "@lucide/svelte";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import type { RetryErrorKind } from "../../lib/manage/retry.ts";
  import { classifyRetryError, relativeTime } from "../../lib/manage/retry.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import { navigate } from "../../lib/shell/port.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import AgentIdentityDialog from "../../components/manage/AgentIdentityDialog.svelte";
  import CopyButton from "../../components/CopyButton.svelte";
  import StateDot from "../../components/manage/StateDot.svelte";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import CurrentProfileCard from "../../components/manage/CurrentProfileCard.svelte";
  import { getLocale } from "../../paraglide/runtime.js";

  import {
    common_edit,
    common_remove,
    manage_agent_title,
    manage_agent_back,
    manage_agent_role,
    manage_agent_duty,
    manage_agent_identity_actions_aria,
    manage_agent_duty_onduty,
    manage_agent_duty_offduty,
    agent_state_idle,
    agent_state_busy,
    agent_state_faulted,
    manage_agent_created_by,
    manage_agent_workspace,
    manage_agent_description,
    manage_agent_context_usage,
    manage_agent_context_label,
    manage_agent_context_tokens,
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
    manage_agent_retry_show_relative,
    manage_agent_retry_show_less,
    manage_agent_retry_show_absolute,
    manage_agent_retry_show_all,
    manage_agent_retry_just_now,
    manage_agent_retry_minutes_ago,
    manage_agent_retry_hours_ago,
    manage_agent_retry_days_ago,
    manage_agent_retry_error_network,
    manage_agent_retry_error_timeout,
    manage_agent_retry_error_rate_limit,
    manage_agent_retry_error_auth,
    manage_agent_retry_error_unknown,
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

  let showRemoveDialog = $state(false);
  let showIdentityDialog = $state(false);
  let retryMode = $state<"relative" | "absolute">("relative");
  let showAllRetries = $state(false);

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

  interface RetryEntry {
    timestamp: number;
    attempt: number;
    max_attempts: number;
    error: string;
  }

  function retryOutcome(r: RetryEntry): string {
    return r.attempt < r.max_attempts
      ? manage_agent_retry_retried()
      : manage_agent_retry_exhausted();
  }

  function fmtAbsoluteRetry(r: RetryEntry): string {
    return manage_agent_retry_line({
      date: new Date(r.timestamp * 1000).toLocaleString(getLocale()),
      error: r.error,
      outcome: retryOutcome(r),
    });
  }

  // Display labels for the classified error kinds (retry.ts owns the
  // string->kind mapping; this maps kind->message).
  const errorLabels: Record<RetryErrorKind, () => string> = {
    network: manage_agent_retry_error_network,
    timeout: manage_agent_retry_error_timeout,
    rate_limit: manage_agent_retry_error_rate_limit,
    auth: manage_agent_retry_error_auth,
    unknown: manage_agent_retry_error_unknown,
  };

  function fmtRelativeRetry(r: RetryEntry): string {
    // Date.now() is read per render: the 5s status poll re-renders the
    // card, so the relative bucket refreshes on the data's own cadence.
    const { kind, n } = relativeTime(
      Math.floor(Date.now() / 1000),
      r.timestamp,
    );
    const date =
      kind === "just"
        ? manage_agent_retry_just_now()
        : kind === "min"
          ? manage_agent_retry_minutes_ago({ n })
          : kind === "hour"
            ? manage_agent_retry_hours_ago({ n })
            : manage_agent_retry_days_ago({ n });
    return manage_agent_retry_line({
      date,
      error: errorLabels[classifyRetryError(r.error)](),
      outcome: retryOutcome(r),
    });
  }

  async function onSaveIdentity(role: string, description: string) {
    await agentsStore
      .updateMetadata(id, { role, description })
      .catch(() => {});
    showIdentityDialog = false;
  }
  async function onConfirmRemove() {
    await agentsStore.remove(id).catch(() => {});
    showRemoveDialog = false;
    navigate("/local/manage/agents");
  }
</script>

<svelte:head><title>{manage_agent_title({ id: agent?.role || id })}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div>
      <div class="flex items-center gap-3">
        <a
          href={`${basePath}/agents`}
          class="btn btn-sm preset-outlined-surface-500">{manage_agent_back()}</a
        >
        {#if agent}
          <h1 class="text-xl font-semibold truncate min-w-0">{agent.role || "—"}</h1>
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
        {:else}
          <h1 class="text-xl font-semibold font-mono truncate min-w-0">{id}</h1>
        {/if}
      </div>
      {#if agent}
        <div class="flex items-start gap-1 mt-1 group">
          <p class="font-mono text-xs opacity-60 break-all select-text min-w-0">{id}</p>
          <CopyButton getText={() => id} />
        </div>
      {/if}
    </div>

    {#if statusError}
      <p class="text-error-500 dark:text-error-400 text-sm">{statusError}</p>
    {/if}

    {#if agent}
      <section class="card preset-tonal-surface p-5 space-y-3">
        <div class="flex justify-end">
          <Menu
            positioning={{ placement: "bottom-end" }}
            onSelect={(e) => {
              if (e.value === "edit") showIdentityDialog = true;
              else if (e.value === "remove") showRemoveDialog = true;
            }}
          >
            <Menu.Trigger
              class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
              aria-label={manage_agent_identity_actions_aria()}
            >
              <MoreVertical class="size-4" />
            </Menu.Trigger>
            <Portal>
              <Menu.Positioner>
                <Menu.Content
                  class="card preset-tonal-surface p-1 min-w-[8rem]"
                >
                  <Menu.Item
                    value="edit"
                    class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                  >
                    <Pencil class="size-4" />
                    {common_edit()}
                  </Menu.Item>
                  <Menu.Item
                    value="remove"
                    class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                  >
                    <Trash class="size-4" />
                    {manage_agent_remove_agent()}
                  </Menu.Item>
                </Menu.Content>
              </Menu.Positioner>
            </Portal>
          </Menu>
        </div>
        <div class="grid grid-cols-2 gap-3 text-sm">
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_role()}</span
            >
            <span>{agent.role || "—"}</span>
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
            <span class="font-mono text-xs" title={agent.created_by ?? "root"}>{agent.created_by ?? "root"}</span>
          </div>
          <div>
            <span class="opacity-60 text-xs uppercase tracking-wide block"
              >{manage_agent_workspace()}</span
            >
            <span
              class="font-mono text-xs truncate block"
              title={agent.workspace_root}
              >{agent.workspace_root}</span
            >
          </div>
        </div>
        <div>
          <span class="opacity-60 text-xs uppercase tracking-wide block"
            >{manage_agent_description()}</span
          >
          <span class="text-sm">{agent.description || "—"}</span>
        </div>
      </section>
    {/if}
    {#if status?.profile}
      <CurrentProfileCard profile={status.profile} />
    {/if}

    {#if status}
      <section class="card preset-tonal-surface p-5 space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_agent_context_usage()}
        </h2>
        <BudgetBar
          consumed={cumulativeTokens}
          budget={status.token_budget}
          label={manage_agent_context_label()}
        />
        <p class="text-xs opacity-70">
          {manage_agent_context_tokens({
            consumed: formatTokenCount(cumulativeTokens),
            budget: formatTokenCount(status.token_budget),
          })}
        </p>
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
          <div class="flex items-center justify-between gap-2">
            <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
              {manage_agent_recent_retries()}
            </h2>
            <button
              type="button"
              aria-pressed={retryMode === "relative"}
              title={retryMode === "relative"
                ? manage_agent_retry_show_absolute()
                : manage_agent_retry_show_relative()}
              aria-label={retryMode === "relative"
                ? manage_agent_retry_show_absolute()
                : manage_agent_retry_show_relative()}
              onclick={() =>
                retryMode = retryMode === "relative" ? "absolute" : "relative"}
              class="rounded p-1.5 text-surface-500 dark:text-surface-400 hover:bg-surface-200-800 transition"
            >
              {#if retryMode === "relative"}
                <CalendarClock class="size-4" />
              {:else}
                <Clock class="size-4" />
              {/if}
            </button>
          </div>
          {#each (showAllRetries
              ? status.recent_retries
              : status.recent_retries.slice(0, 3)) as r}
            <div class="text-xs opacity-70">
              {retryMode === "relative"
                ? fmtRelativeRetry(r)
                : fmtAbsoluteRetry(r)}
            </div>
          {/each}
          {#if status.recent_retries.length > 3}
            <button
              type="button"
              onclick={() => (showAllRetries = !showAllRetries)}
              class="text-xs opacity-60 hover:opacity-100 transition"
            >
              {showAllRetries
                ? manage_agent_retry_show_less()
                : manage_agent_retry_show_all({
                  count: status.recent_retries.length,
                })}
            </button>
          {/if}
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
  <AgentIdentityDialog
    open={showIdentityDialog}
    role={agent?.role ?? ""}
    description={agent?.description ?? ""}
    busy={agentsStore.isInFlight(id)}
    onSave={onSaveIdentity}
    onCancel={() => (showIdentityDialog = false)}
  />
