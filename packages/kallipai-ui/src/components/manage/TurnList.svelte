<script lang="ts">
  // The left column: one row per turn, plus the list-level states (loading,
  // no log, a failed read, a newer face, nothing recorded, the page remarks).
  // Rows carry only what a reader triages by -- which turn, when, on which
  // model, how long, what the turn was, and how many of its calls failed.
  import type { TurnRow } from "@kallipai/kallipai-client";
  import { stepIndex } from "../../lib/manage/turn_nav.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";
  import {
    type CardError,
    callStatus,
    type Notice,
    turnShape,
  } from "../../lib/manage/turnView.ts";
  import {
    manage_history_bad_lines_one,
    manage_history_bad_lines_other,
    manage_history_call_failed_count,
    manage_history_calls_label,
    manage_history_elapsed_value,
    manage_history_empty,
    manage_history_load_failed,
    manage_history_load_more,
    manage_history_loading,
    manage_history_no_log,
    manage_history_page_cut,
    manage_history_refresh,
    manage_history_schema_face,
    manage_history_shape_assistant,
    manage_history_shape_user,
  } from "../../paraglide/messages.js";

  let {
    rows,
    notes,
    selectedId,
    loading,
    fire,
    error,
    face,
    moreError,
    canMore,
    loadingMore,
    refreshError,
    onSelect,
    onMore,
    onRetry,
    onRefresh,
  }: {
    rows: TurnRow[];
    notes: Notice[];
    selectedId: number | null;
    loading: boolean;
    fire: CardError;
    error: string | null;
    face: number | null;
    moreError: string | null;
    canMore: boolean;
    loadingMore: boolean;
    refreshError: string | null;
    onSelect: (turnId: number, viaKeyboard: boolean) => void;
    onMore: () => void;
    onRetry: () => void;
    onRefresh: () => void;
  } = $props();

  function noticeText(notice: Notice): string {
    switch (notice.kind) {
      case "bad-lines":
        return notice.count === 1
          ? manage_history_bad_lines_one({ count: notice.count })
          : manage_history_bad_lines_other({ count: notice.count });
      case "page-cut":
        return manage_history_page_cut();
      case "face":
        return manage_history_schema_face();
      default: {
        // A new notice kind without a line here is a compile error, not a
        // blank row.
        const unread: never = notice;
        return unread;
      }
    }
  }

  /** The rows in the order they are drawn, so a key press can name the one
   * under the focus and the one after it. */
  let listEl = $state<HTMLUListElement | null>(null);

  /** Arrow keys walk the rows and the browser scrolls the row it lands on into
   * view, so the eye follows the focus. Opening stays the row's own act
   * (Enter or Space), so a walk down a long list does not read every turn. */
  function onKey(event: KeyboardEvent): void {
    const buttons = [
      ...(listEl?.querySelectorAll<HTMLButtonElement>("button[data-turn]") ??
        []),
    ];
    const at = buttons.findIndex((button) => button === document.activeElement);
    const target = stepIndex(at, buttons.length, event.key);
    const next = target === null ? undefined : buttons[target];
    if (next === undefined) return;
    event.preventDefault();
    next.focus();
  }

  const failedCount = (row: TurnRow) =>
    row.calls.filter((call) => callStatus(call) === "failed").length;
</script>

<div class="space-y-2">
  {#if refreshError}
    <p class="text-xs text-error-500">{refreshError}</p>
  {/if}
  {#if loading && rows.length === 0}
    <p class="text-xs opacity-70">{manage_history_loading()}</p>
  {:else if fire === "no-log"}
    <p class="text-xs opacity-70">{manage_history_no_log()}</p>
  {:else if fire === "failed"}
    <p class="text-xs text-error-500">
      {error ?? manage_history_load_failed()}
    </p>
    <button
      type="button"
      class="text-xs opacity-60 transition hover:opacity-100"
      disabled={loading}
      onclick={onRetry}>{manage_history_refresh()}</button
    >
  {:else if face !== null || rows.length === 0}
    {#each notes as notice}
      <p class="text-xs opacity-60">{noticeText(notice)}</p>
    {/each}
    {#if face === null}
      <p class="text-xs opacity-70">{manage_history_empty()}</p>
    {/if}
    <button
      type="button"
      class="text-xs opacity-60 transition hover:opacity-100"
      disabled={loading}
      onclick={onRefresh}>{manage_history_refresh()}</button
    >
  {:else}
    {#each notes as notice}
      <p class="text-xs opacity-60">{noticeText(notice)}</p>
    {/each}
    <ul class="space-y-1" bind:this={listEl}>
      {#each rows as row (row.turn_id)}
        {@const shape = turnShape(row)}
        <li>
          <button
            type="button"
            class="w-full space-y-0.5 rounded p-2 text-left transition {selectedId ===
            row.turn_id
              ? 'bg-surface-100-900 ring-1 ring-inset ring-primary-800-200'
              : 'hover:bg-surface-100-900'}"
            aria-current={selectedId === row.turn_id ? "true" : undefined}
            data-turn={row.turn_id}
            onkeydown={onKey}
            onclick={(event) =>
              // A key press reaches a button as a click with no count:
              // detail 0. Only a pointer click carries one.
              onSelect(row.turn_id, event.detail === 0)}
          >
            <div class="flex items-center gap-2 text-xs">
              <span class="font-mono font-medium">#{row.turn_id}</span>
              <span class="ml-auto text-surface-800-200"
                >{formatStamp(row.at)}</span
              >
            </div>
            {#if row.profile}
              <p class="truncate text-xs text-surface-800-200">
                {row.profile.provider} · {row.profile.model}
              </p>
            {/if}
            <div
              class="flex flex-wrap items-center gap-x-2 text-xs text-surface-800-200"
            >
              {#if row.elapsed_ms !== undefined}
                <span
                  >{manage_history_elapsed_value({ ms: row.elapsed_ms })}</span
                >
              {/if}
              {#if shape === "tools"}
                <span>{manage_history_calls_label()}: {row.calls.length}</span>
              {:else if shape === "user"}
                <span>{manage_history_shape_user()}</span>
              {:else if shape === "assistant"}
                <span>{manage_history_shape_assistant()}</span>
              {/if}
              {#if failedCount(row) > 0}
                <span
                  class="flex items-center gap-1 text-error-700 dark:text-error-100"
                >
                  <span
                    class="size-1.5 rounded-full bg-error-500"
                    aria-hidden="true"
                  ></span>
                  {manage_history_call_failed_count({
                    count: failedCount(row),
                  })}
                </span>
              {/if}
            </div>
          </button>
        </li>
      {/each}
    </ul>
    {#if moreError}
      <p class="text-xs text-error-500">{moreError}</p>
    {/if}
    {#if canMore}
      <button
        type="button"
        class="text-xs opacity-60 transition hover:opacity-100"
        disabled={loading || loadingMore}
        onclick={onMore}>{manage_history_load_more()}</button
      >
    {/if}
  {/if}
</div>
