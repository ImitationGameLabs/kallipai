<script lang="ts">
  // One files-page row: display path, compact size/date pair, and the two
  // row actions (download, delete). Presentational: the dashboard owns the
  // I/O and passes handlers in; a failed action surfaces inline here.
  import { Download, Trash2 } from "@lucide/svelte";
  import {
    common_delete,
    files_download_aria,
  } from "../../paraglide/messages.js";
  import type { FileRowView } from "../../lib/filesView.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";

  let {
    row,
    highlighted = false,
    error = null,
    onDownload,
    onDelete,
  }: {
    row: FileRowView;
    highlighted?: boolean;
    error?: string | null;
    onDownload: (row: FileRowView) => void;
    onDelete: (row: FileRowView) => void;
  } = $props();

  // One decimal past the unit boundary is plenty for a list scan.
  const sizeLabel = $derived(
    row.size < 1024
      ? `${row.size} B`
      : row.size < 1024 * 1024
        ? `${(row.size / 1024).toFixed(1)} KB`
        : `${(row.size / (1024 * 1024)).toFixed(1)} MB`,
  );
  const dateLabel = $derived(
    formatStamp(row.createdAt, {
      year: "numeric",
      month: "short",
      day: "numeric",
    }),
  );
</script>

<div
  class="px-3 py-1.5 md:px-4 md:py-2 flex flex-col gap-1 {highlighted
    ? 'bg-primary-100-900'
    : ''}"
>
  <div class="flex items-center gap-2 min-w-0">
    <div class="min-w-0 flex-1">
      <div class="text-sm truncate" title={row.displayPath}>
        {row.displayPath}
      </div>
      <div class="text-xs opacity-60">{sizeLabel} · {dateLabel}</div>
    </div>
    <button
      type="button"
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
      aria-label={files_download_aria({ name: row.displayPath })}
      onclick={() => onDownload(row)}
    >
      <Download class="size-4" />
    </button>
    <button
      type="button"
      class="btn btn-sm preset-outlined-error-500 hover:preset-filled-error-500"
      aria-label={common_delete()}
      onclick={() => onDelete(row)}
    >
      <Trash2 class="size-4" />
    </button>
  </div>
  {#if error}
    <p class="text-xs text-error-500 dark:text-error-400">{error}</p>
  {/if}
</div>
