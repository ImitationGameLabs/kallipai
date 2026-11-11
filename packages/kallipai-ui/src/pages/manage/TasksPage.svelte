<script lang="ts">
  // The task ledger page (read-only first version): a two-pane view —
  // the list on the left, the selected task's trail on the right. Write
  // verbs stay CLI-only (actor identity is the tagma's law); this page
  // points at the CLI instead of offering buttons.
  import { tasksStore } from "../../lib/manage/tasks.svelte.ts";
  import {
    type ConfirmationState,
    confirmerConfirmationState,
    confirmerRoleMap,
    associationText,
    timelinePayloadView,
    durationState,
    formatDuration,
    formatTokens,
    roleLabel,
  } from "../../lib/compute.ts";
  import {
    common_loading,
    manage_tasks_title,
    manage_tasks_heading,
    manage_tasks_empty,
    manage_tasks_col_assignee,
    manage_tasks_col_created,
    manage_tasks_show_archived,
    manage_tasks_readonly,
    manage_tasks_timeline,
    manage_tasks_confirmers,
    manage_tasks_confirm_done,
    manage_tasks_confirm_pending,
    manage_tasks_no_confirmers,
    manage_tasks_close_reason,
    manage_tasks_confirm_no_report,
    manage_tasks_page_prev,
    manage_tasks_page_next,
    manage_tasks_page_status,
    manage_tasks_filter_all,
    manage_tasks_filter_time,
    manage_tasks_filter_since,
    manage_tasks_filter_until,
    manage_tasks_col_updated,
    manage_tasks_close_summary,
    manage_tasks_has_reports,
    manage_tasks_creator,
    manage_tasks_col_started,
    manage_tasks_col_ended,
    manage_tasks_col_archived,
    manage_tasks_archive_hash,
    manage_tasks_association,
    manage_tasks_download_archive,
    manage_tasks_export_json,
    manage_tasks_export_all,
    manage_tasks_downloads_offline,
    manage_tasks_duration,
    manage_tasks_duration_note,
    manage_tasks_duration_open,
    manage_tasks_usage,
    manage_tasks_usage_note,
    manage_tasks_usage_none,
    manage_tasks_usage_partial,
  } from "../../paraglide/messages.js";
  import type { TaskStatus, TaskTimeAxis } from "@kallipai/kallipai-client";
  import { TASKS_PAGE_SIZE } from "../../lib/manage/tasks.svelte.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";
  import { STAMP_SECONDS } from "../../lib/time/stamp.ts";
  import { getLocale } from "../../paraglide/runtime.js";

  $effect(() => {
    tasksStore.startPolling(30_000);
    return () => tasksStore.stopPolling();
  });

  // The status color encodes data (a five-vocabulary state machine), not
  // UI chrome, so raw utilities instead of a surface preset.
  const statusColor: Record<string, string> = {
    queued: "bg-surface-500",
    in_progress: "bg-primary-500",
    paused: "bg-tertiary-500",
    review: "bg-warning-500",
    closed: "bg-success-500",
  };

  function fmtTime(iso: string | null): string {
    if (!iso) return "—";
    return formatStamp(iso, STAMP_SECONDS);
  }

  // One merged stream, oldest first: transitions and actions are both
  // trail rows and read naturally interleaved (GitHub-issue style).
  const timeline = $derived(
    tasksStore.detail
      ? [...tasksStore.detail.events].sort((a, b) =>
          (a.created_at ?? "").localeCompare(b.created_at ?? ""),
        )
      : [],
  );

  // Confirmer → confirmation state in the current cycle: a confirmation
  // counts only past the start/reopen boundary (gates.rs semantics),
  // keyed by the raw actor id — the same id space the confirmers
  // roster uses. Missing confirmations are the close gate's
  // outstanding count, readable at a glance.
  const confirmationByConfirmer = $derived(
    tasksStore.detail
      ? confirmerConfirmationState(
          tasksStore.detail.events,
          tasksStore.detail.confirmers,
        )
      : new Map<string, ConfirmationState>(),
  );

  // Headline vote badge: filed confirmations over the registered roster.
  const confirmedCount = $derived(
    [...confirmationByConfirmer.values()].filter((c) => c.filed).length,
  );

  // The detail pane's own numbers: both come from stamps the ledger already
  // carries, so nothing is captured twice and nothing ticks on its own.
  // A task that never started has no span, and stamps that will not parse
  // read as unknown rather than open; only a start without an end is still
  // on its wall clock.
  const duration = $derived(
    durationState(
      tasksStore.detail?.started_at ?? null,
      tasksStore.detail?.ended_at ?? null,
    ),
  );
  const durationText = $derived(
    duration.kind === "span"
      ? formatDuration(duration.ms)
      : duration.kind === "open"
        ? manage_tasks_duration_open()
        : "—",
  );
  const usage = $derived(tasksStore.detail?.usage ?? null);

  // Confirmer identity pairs by roster index: entry N goes with
  // confirmer_roles[N], whatever order the mount loop reaches them.
  const confirmerRoles = $derived(
    confirmerRoleMap(
      tasksStore.detail?.confirmers ?? [],
      tasksStore.detail?.confirmer_roles,
    ),
  );

  const pages = $derived(Math.ceil(tasksStore.total / TASKS_PAGE_SIZE));

  // Filter chips: the five-vocabulary state machine plus the all row.
  const statusChips: (TaskStatus | null)[] = [
    null,
    "queued",
    "in_progress",
    "paused",
    "review",
    "closed",
  ];

  // The one expanded confirmation row (single-open accordion): report
  // can be long, so only one rolls open at a time.
  let expandedConfirmer = $state<string | null>(null);

  // Downloads need the direct (offline) transport; over the relay tunnel
  // the manage-reply frame caps response bodies, so the buttons disable
  // and the hint points at the CLI instead.
  function saveBlob(
    data: string | Uint8Array,
    filename: string,
    type: string,
  ): void {
    // The DOM lib types the Blob part as narrow overloads; the cast
    // silences that lib noise, not a real risk - do not simplify away.
    const url = URL.createObjectURL(new Blob([data as BlobPart], { type }));
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    a.click();
    URL.revokeObjectURL(url);
  }

  async function downloadArchive(id: number): Promise<void> {
    const bytes = await tasksStore.fetchArchive(id);
    if (bytes) {
      saveBlob(bytes, `task-${id}-archive.tar`, "application/x-tar");
    }
  }

  async function exportTaskJson(id: number): Promise<void> {
    const task = await tasksStore.exportTask(id);
    if (task) {
      saveBlob(
        JSON.stringify(task, null, 2),
        `task-${id}-export.json`,
        "application/json",
      );
    }
  }

  async function exportAllJson(): Promise<void> {
    const all = await tasksStore.exportAllTasks();
    if (all) {
      saveBlob(
        JSON.stringify(all, null, 2),
        "tasks-export.json",
        "application/json",
      );
    }
  }
</script>

<svelte:head><title>{manage_tasks_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 md:p-6 space-y-4">
    <div class="flex items-center justify-between">
      <h1 class="text-xl font-semibold hidden md:block">
        {manage_tasks_heading()}
      </h1>
      <div class="flex items-center gap-3">
        <label class="flex items-center gap-1.5 text-sm">
          <input
            type="checkbox"
            class="checkbox"
            checked={tasksStore.showArchived}
            onchange={() => tasksStore.toggleArchived()}
          />
          {manage_tasks_show_archived()}
        </label>
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          disabled={!tasksStore.downloadsAvailable || tasksStore.downloading}
          onclick={() => exportAllJson()}
        >
          {manage_tasks_export_all()}
        </button>
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => tasksStore.refresh(true)}>⟳</button
        >
      </div>
    </div>

    <p class="text-xs opacity-60 preset-tonal-surface rounded px-3 py-2">
      {manage_tasks_readonly()}
    </p>

    <div class="flex flex-wrap items-center gap-1.5">
      {#each statusChips as chip (chip)}
        <button
          type="button"
          class="btn btn-sm {tasksStore.statusFilter === chip
            ? 'preset-filled-primary-500'
            : 'preset-outlined-surface-500 hover:preset-filled-surface-500'}"
          onclick={() => tasksStore.setStatus(chip)}
        >
          {#if chip === null}{manage_tasks_filter_all()}{:else}{chip}{/if}
        </button>
      {/each}
    </div>

    <div class="flex flex-wrap items-end gap-3 text-sm">
      <label class="flex flex-col gap-1">
        <span class="text-xs opacity-60">{manage_tasks_col_assignee()}</span>
        <input
          class="input h-8 w-40"
          value={tasksStore.assigneeFilter}
          onchange={(e) => tasksStore.setAssignee(e.currentTarget.value)}
        />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs opacity-60">{manage_tasks_filter_time()}</span>
        <select
          class="select h-8 w-32"
          value={tasksStore.timeAxis}
          onchange={(e) =>
            tasksStore.setTimeAxis(e.currentTarget.value as TaskTimeAxis)}
        >
          <option value="updated">updated</option>
          <option value="closed">closed</option>
        </select>
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs opacity-60">{manage_tasks_filter_since()}</span>
        <input
          type="date"
          class="input h-8 w-36"
          value={tasksStore.sinceDate}
          onchange={(e) => tasksStore.setSince(e.currentTarget.value)}
        />
      </label>
      <label class="flex flex-col gap-1">
        <span class="text-xs opacity-60">{manage_tasks_filter_until()}</span>
        <input
          type="date"
          class="input h-8 w-36"
          value={tasksStore.untilDate}
          onchange={(e) => tasksStore.setUntil(e.currentTarget.value)}
        />
      </label>
    </div>

    {#if !tasksStore.downloadsAvailable}
      <p class="text-xs opacity-60 preset-tonal-surface rounded px-3 py-2">
        {manage_tasks_downloads_offline()}
      </p>
    {/if}

    {#if tasksStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {tasksStore.error}
      </p>
    {/if}

    {#if tasksStore.isLoading && !tasksStore.hasLoaded}
      <p class="opacity-60 text-sm">{common_loading()}</p>
    {:else if tasksStore.rows.length === 0}
      <p class="opacity-60 text-sm">{manage_tasks_empty()}</p>
    {:else}
      <div class="flex flex-col lg:flex-row gap-4">
        <!-- list pane -->
        <div class="lg:w-2/5 space-y-1">
          {#each tasksStore.rows as task (task.id)}
            <button
              type="button"
              class="w-full text-left card preset-tonal-surface p-3 {tasksStore.selectedId ===
              task.id
                ? 'ring-2 ring-primary-500'
                : 'hover:preset-outlined-surface-500'}"
              onclick={() => tasksStore.select(task.id)}
            >
              <div class="flex items-center gap-2">
                <span
                  class="size-2 rounded-full {statusColor[task.status] ??
                    'bg-surface-500'}"
                ></span>
                <span class="font-mono text-xs opacity-60">#{task.id}</span>
                <span class="text-sm font-medium truncate flex-1"
                  >{task.title}</span
                >
              </div>
              <div class="flex gap-3 text-xs opacity-60 mt-1 flex-wrap">
                <span>{task.status}</span>
                <span>{roleLabel(task.assignee, task.assignee_role)}</span>
                <span>{fmtTime(task.created_at)}</span>
                <span
                  >{manage_tasks_col_updated()}:
                  {fmtTime(task.updated_at)}</span
                >
                <span
                  >{manage_tasks_confirmers()}:
                  {task.confirmers.length}</span
                >
                {#if task.has_reports}
                  <span class="text-success-500">
                    ✓ {manage_tasks_has_reports()}
                  </span>
                {/if}
              </div>
            </button>
          {/each}
          {#if tasksStore.total > 0}
            <div class="flex items-center gap-2 text-xs px-1 pt-1">
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={tasksStore.page === 0}
                onclick={() => tasksStore.goToPage(tasksStore.page - 1)}
              >
                {manage_tasks_page_prev()}
              </button>
              <span class="opacity-80">
                {manage_tasks_page_status({
                  page: tasksStore.page + 1,
                  pages,
                  total: tasksStore.total,
                })}
              </span>
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={tasksStore.page >= pages - 1}
                onclick={() => tasksStore.goToPage(tasksStore.page + 1)}
              >
                {manage_tasks_page_next()}
              </button>
            </div>
          {/if}
        </div>

        <!-- detail pane -->
        <div class="lg:w-3/5">
          {#if tasksStore.detail}
            <div class="card preset-tonal-surface p-4 space-y-4">
              <div class="flex items-center gap-2 flex-wrap">
                <span
                  class="size-2 rounded-full {statusColor[
                    tasksStore.detail.status
                  ] ?? 'bg-surface-500'}"
                ></span>
                <span class="font-mono text-xs opacity-60"
                  >#{tasksStore.detail.id}</span
                >
                <h2 class="text-base font-semibold flex-1">
                  {tasksStore.detail.title}
                </h2>
                {#if tasksStore.detail.confirmers.length > 0}
                  <span
                    class="text-xs preset-filled-success-500 rounded-full px-2 py-0.5"
                  >
                    {confirmedCount}/{tasksStore.detail.confirmers.length}
                  </span>
                {/if}
                <div class="flex gap-1.5">
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                    disabled={!tasksStore.downloadsAvailable ||
                      tasksStore.downloading ||
                      !tasksStore.detail.archived}
                    onclick={() => downloadArchive(tasksStore.detail!.id)}
                  >
                    {manage_tasks_download_archive()}
                  </button>
                  <button
                    type="button"
                    class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
                    disabled={!tasksStore.downloadsAvailable ||
                      tasksStore.downloading}
                    onclick={() => exportTaskJson(tasksStore.detail!.id)}
                  >
                    {manage_tasks_export_json()}
                  </button>
                </div>
              </div>

              <div class="text-xs opacity-60 flex gap-4 flex-wrap">
                <span
                  >{manage_tasks_col_assignee()}:
                  {roleLabel(
                    tasksStore.detail.assignee,
                    tasksStore.detail.assignee_role,
                  )}</span
                >
                <span
                  >{manage_tasks_col_created()}:
                  {fmtTime(tasksStore.detail.created_at)}</span
                >
                {#if tasksStore.detail.creator}
                  <span
                    >{manage_tasks_creator()}:
                    {roleLabel(
                      tasksStore.detail.creator,
                      tasksStore.detail.creator_role,
                    )}</span
                  >
                {/if}
                {#if tasksStore.detail.started_at}
                  <span
                    >{manage_tasks_col_started()}:
                    {fmtTime(tasksStore.detail.started_at)}</span
                  >
                {/if}
                {#if tasksStore.detail.ended_at}
                  <span
                    >{manage_tasks_col_ended()}:
                    {fmtTime(tasksStore.detail.ended_at)}</span
                  >
                {/if}
                {#if tasksStore.detail.archived_at}
                  <span
                    >{manage_tasks_col_archived()}:
                    {fmtTime(tasksStore.detail.archived_at)}</span
                  >
                {/if}
                {#if tasksStore.detail.closed_reason}
                  <span
                    >{manage_tasks_close_reason()}:
                    <span class="font-mono"
                      >{tasksStore.detail.closed_reason}</span
                    ></span
                  >
                {/if}
                {#if tasksStore.detail.archive_hash}
                  <span
                    >{manage_tasks_archive_hash()}:
                    <span class="font-mono"
                      >{tasksStore.detail.archive_hash.slice(0, 12)}…</span
                    ></span
                  >
                {/if}
                {#if tasksStore.detail.association}
                  <span
                    >{manage_tasks_association()}:
                    <span class="font-mono"
                      >{associationText(tasksStore.detail.association)}</span
                    ></span
                  >
                {/if}
                {#if tasksStore.detail.close_summary}
                  <span
                    >{manage_tasks_close_summary()}:
                    {tasksStore.detail.close_summary}</span
                  >
                {/if}
              </div>

              <!-- duration and token spend: numbers plus their scope -->
              <div class="grid gap-2 sm:grid-cols-2">
                <div>
                  <p class="text-xs opacity-60">{manage_tasks_duration()}</p>
                  <p class="text-sm">
                    {durationText}
                  </p>
                  <p class="text-xs opacity-60">
                    {manage_tasks_duration_note()}
                  </p>
                </div>
                <div>
                  <p class="text-xs opacity-60">{manage_tasks_usage()}</p>
                  <p class="text-sm">
                    {usage === null
                      ? manage_tasks_usage_none()
                      : formatTokens(usage.total_tokens, getLocale())}
                  </p>
                  {#if usage !== null}
                    <p class="text-xs opacity-60">
                      {manage_tasks_usage_note()}
                    </p>
                  {/if}
                  {#if usage !== null && usage.partial}
                    <p class="text-xs text-warning-500 dark:text-warning-400">
                      {manage_tasks_usage_partial()}
                    </p>
                  {/if}
                </div>
              </div>

              <!-- confirmers: the close gate at a glance -->
              <div>
                <h3 class="text-sm font-medium mb-1">
                  {manage_tasks_confirmers()}
                </h3>
                {#if confirmationByConfirmer.size === 0}
                  <p class="text-xs opacity-60">
                    {manage_tasks_no_confirmers()}
                  </p>
                {:else}
                  <ul class="space-y-0.5">
                    {#each [...confirmationByConfirmer] as [confirmer, confirmation] (confirmer)}
                      <li class="text-sm">
                        <div class="flex items-center gap-2">
                          {#if confirmation.filed}
                            <button
                              type="button"
                              class="flex items-center gap-2 text-left"
                              onclick={() =>
                                (expandedConfirmer =
                                  expandedConfirmer === confirmer
                                    ? null
                                    : confirmer)}
                            >
                              <span class="text-success-500">✓</span>
                              <span class="font-mono text-xs"
                                >{roleLabel(
                                  confirmer,
                                  confirmerRoles.get(confirmer),
                                )}
                              </span>
                              <span class="text-xs opacity-60"
                                >{manage_tasks_confirm_done()}</span
                              >
                              <span class="text-xs opacity-40"
                                >{expandedConfirmer === confirmer
                                  ? "▾"
                                  : "▸"}</span
                              >
                            </button>
                          {:else}
                            <span class="opacity-40">○</span>
                            <span class="font-mono text-xs"
                              >{roleLabel(
                                confirmer,
                                confirmerRoles.get(confirmer),
                              )}
                            </span>
                            <span class="text-xs opacity-60"
                              >{manage_tasks_confirm_pending()}</span
                            >
                          {/if}
                        </div>
                        {#if confirmation.filed && expandedConfirmer === confirmer}
                          <div class="mt-1 ml-6 space-y-1">
                            <p class="text-xs opacity-60">
                              v{confirmation.version} · {fmtTime(
                                confirmation.reportedAt,
                              )}
                            </p>
                            {#if confirmation.report === null}
                              <p class="text-xs opacity-60">
                                {manage_tasks_confirm_no_report()}
                              </p>
                            {:else}
                              <pre
                                class="text-xs whitespace-pre-wrap break-words preset-tonal-surface rounded p-2 max-h-64 overflow-y-auto">{confirmation.report}</pre>
                            {/if}
                          </div>
                        {/if}
                      </li>
                    {/each}
                  </ul>
                {/if}
              </div>

              <!-- single interleaved trail -->
              <div>
                <h3 class="text-sm font-medium mb-1">
                  {manage_tasks_timeline()}
                </h3>
                <ol class="space-y-1.5">
                  {#each timeline as event (event.id)}
                    {@const pv = timelinePayloadView(event.payload)}
                    <li class="text-xs flex gap-2 items-baseline flex-wrap">
                      <span class="opacity-50 whitespace-nowrap"
                        >{fmtTime(event.created_at)}</span
                      >
                      <span
                        class="font-medium {pv.forced
                          ? 'text-warning-500'
                          : ''}">{event.name}</span
                      >
                      {#if event.from_status && event.to_status}
                        <span class="opacity-60"
                          >{event.from_status} → {event.to_status}</span
                        >
                      {/if}
                      {#if event.actor}
                        <span class="opacity-60 font-mono"
                          >{roleLabel(event.actor, event.actor_role)}</span
                        >
                      {/if}
                      {#if pv.detail}
                        <span class="opacity-50 font-mono">{pv.detail}</span>
                      {/if}
                      {#if pv.note}
                        <pre
                          class="w-full text-xs whitespace-pre-wrap break-words preset-tonal-surface rounded p-2">{pv.note}</pre>
                      {/if}
                    </li>
                  {/each}
                </ol>
              </div>
            </div>
          {/if}
        </div>
      </div>
    {/if}
  </div>
</div>
