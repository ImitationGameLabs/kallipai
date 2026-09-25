<script lang="ts">
  // The files page surface: one list(space=self, limit=500)
  // call, client-side grouping and prefix filter (filesView), download via
  // the shared saveBlob, delete behind ConfirmDialog, upload through a
  // small dialog (read-only name, area dropdown defaulting to shared/).
  // No optimistic updates: every mutation reloads the list -- at the
  // 500 cap the refetch is cheap and the state stays simple. Row density
  // and dialog width ride the CSS 48rem variants (the one breakpoint).
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { onMount } from "svelte";
  import {
    FilesApiError,
    type FileEntryView,
    type PutResponse,
  } from "@kallipai/kallipai-files-client";
  import {
    archeionSession,
    filesClientOrFail,
  } from "../../lib/session/archeion.svelte";
  import {
    displayPath,
    groupFileEntries,
    matchesFileFilter,
    type FileGroupView,
    type FileRowView,
  } from "../../lib/filesView.ts";
  import { saveBlob } from "../../lib/saveBlob.ts";
  import ConfirmDialog from "../ConfirmDialog.svelte";
  import FileRow from "./FileRow.svelte";
  import {
    common_cancel,
    common_delete,
    common_loading,
    common_retry,
    files_delete_body,
    files_delete_failed,
    files_delete_title,
    files_download_failed,
    files_empty,
    files_empty_filtered,
    files_error,
    files_filter_clear,
    files_filter_placeholder,
    files_group_inbox,
    files_group_root,
    files_group_shared,
    files_group_tagma,
    files_limit_note,
    files_upload,
    files_upload_failed,
    files_upload_name_label,
    files_upload_prefix_hint,
    files_upload_prefix_label,
    files_upload_title,
  } from "../../paraglide/messages.js";

  // The server's hard list cap: one page, no cursor; ==500 means
  // "maybe more" and the fixed tail note says so instead of faking pages.
  const LIMIT = 500;

  type Phase = "loading" | "error" | "ready";

  let phase = $state<Phase>("loading");
  let entries = $state<FileEntryView[]>([]);
  let filter = $state("");
  // The one inline action error: the failing row keeps its message
  // until the next action or reload replaces it.
  let errorRowId = $state<string | null>(null);
  let errorMessage = $state("");
  let uploadOpen = $state(false);
  let uploading = $state(false);
  let uploadError = $state<string | null>(null);
  let uploadFile = $state<File | null>(null);
  let uploadPrefix = $state("shared/");
  let deleting = $state(false);
  let deleteTarget = $state<FileRowView | null>(null);
  let highlightId = $state<string | null>(null);

  const selfPrefix = $derived(
    archeionSession.user
      ? `/users/${archeionSession.user.user_id}/`
      : // Pre-auth the page sits behind the gate anyway; the prefix only
        // shapes display strings until the first load replaces it.
        "/users/unknown/",
  );
  const filtered = $derived(
    entries.filter((e) =>
      matchesFileFilter(displayPath(e.path, selfPrefix), filter),
    ),
  );
  const groups = $derived(groupFileEntries(filtered, selfPrefix));
  const limitHit = $derived(entries.length >= LIMIT);

  function groupLabel(group: FileGroupView): string {
    if (group.kind === "inbox") return files_group_inbox();
    if (group.kind === "shared") return files_group_shared();
    if (group.kind === "root") return files_group_root();
    if (group.kind === "folder") return group.label ?? "";
    const label = archeionSession.enrolledCards.find(
      (t) => t.tagmaId === group.tagmaId,
    )?.label;
    // Unenrolled tagma groups keep the raw id segment (never explode).
    return files_group_tagma({ label: label ?? group.tagmaId ?? "" });
  }
  // The upload areas: the three server-legal prefixes only, so
  // the dialog cannot emit a path the files service would 400. An
  // unenrolled tagma has no source to appear from; the hint says so.
  function uploadAreas() {
    return [
      { value: "shared/", label: files_group_shared() },
      { value: "inbox/", label: files_group_inbox() },
      ...archeionSession.enrolledCards.map((t) => ({
        value: `tagmas/${t.tagmaId}/`,
        label: files_group_tagma({ label: t.label ?? t.tagmaId }),
      })),
    ];
  }

  function clearRowError(): void {
    errorRowId = null;
    errorMessage = "";
  }

  async function load(): Promise<void> {
    phase = "loading";
    clearRowError();
    try {
      entries = await filesClientOrFail().list({ space: "self", limit: LIMIT });
      phase = "ready";
    } catch {
      phase = "error";
    }
  }

  async function download(row: FileRowView): Promise<void> {
    clearRowError();
    try {
      const bytes = await filesClientOrFail().get(row.id);
      const name = row.displayPath.split("/").pop() ?? row.displayPath;
      saveBlob(new Blob([bytes]), name);
    } catch {
      errorRowId = row.id;
      errorMessage = files_download_failed();
    }
  }

  function askDelete(row: FileRowView): void {
    clearRowError();
    deleteTarget = row;
  }

  async function deleteConfirmed(): Promise<void> {
    if (!deleteTarget) return;
    deleting = true;
    const target = deleteTarget;
    try {
      await filesClientOrFail().delete(target.id);
      deleteTarget = null;
      if (highlightId === target.id) highlightId = null;
      await load();
    } catch {
      errorRowId = target.id;
      errorMessage = files_delete_failed();
      deleteTarget = null;
    } finally {
      deleting = false;
    }
  }

  function openUpload(): void {
    uploadFile = null;
    uploadPrefix = "shared/";
    uploadError = null;
    uploadOpen = true;
  }

  function onFileChosen(event: Event): void {
    const input = event.currentTarget as HTMLInputElement;
    uploadFile = input.files?.[0] ?? null;
  }

  async function submitUpload(): Promise<void> {
    if (!uploadFile) return;
    uploading = true;
    uploadError = null;
    // The dropdown offers only the server's three legal areas (shared/,
    // inbox/, or tagmas/<id>/ for enrolled tagmata); others 400.
    try {
      const minted: PutResponse = await filesClientOrFail().put(
        `${selfPrefix}${uploadPrefix}${uploadFile.name}`,
        uploadFile,
      );
      uploadOpen = false;
      // The highlight is the success signal: yield the filter so the
      // new row is actually visible instead of highlighting into the void.
      filter = "";
      highlightId = minted.record_id;
      await load();
    } catch (error) {
      uploadError =
        error instanceof FilesApiError ? error.message : files_upload_failed();
    } finally {
      uploading = false;
    }
  }

  onMount(() => {
    void load();
  });
</script>

<div class="flex flex-col gap-3">
  <div class="flex items-center gap-2">
    <input
      class="input text-sm flex-1 min-w-0"
      type="text"
      placeholder={files_filter_placeholder()}
      bind:value={filter}
    />
    {#if filter.length > 0}
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500"
        onclick={() => (filter = "")}
      >
        {files_filter_clear()}
      </button>
    {/if}
    <button
      type="button"
      class="btn preset-filled-primary-500"
      onclick={openUpload}
    >
      {files_upload()}
    </button>
  </div>

  {#if phase === "loading"}
    <div
      class="card preset-tonal-surface divide-y divide-surface-200-800"
      aria-busy="true"
    >
      {#each [0, 1, 2, 3] as i (i)}
        <div class="px-4 py-3 animate-pulse">
          <div class="h-3 w-1/3 rounded bg-surface-200-800"></div>
        </div>
      {/each}
      <span class="sr-only">{common_loading()}</span>
    </div>
  {:else if phase === "error"}
    <div
      class="card preset-tonal-surface px-4 py-6 flex flex-col items-center gap-3"
    >
      <p class="text-sm opacity-80">{files_error()}</p>
      <button
        type="button"
        class="btn preset-filled-primary-500"
        onclick={() => void load()}
      >
        {common_retry()}
      </button>
    </div>
  {:else if entries.length === 0}
    <div class="card preset-tonal-surface px-4 py-6 text-sm opacity-70">
      {files_empty()}
    </div>
  {:else if filtered.length === 0}
    <div class="card preset-tonal-surface px-4 py-6 text-sm opacity-70">
      {files_empty_filtered()}
    </div>
  {:else}
    {#each groups as group (group.kind + (group.tagmaId ?? group.label ?? ""))}
      <section aria-label={groupLabel(group)}>
        <h3
          class="text-sm font-semibold uppercase tracking-wide opacity-60 px-1"
        >
          {groupLabel(group)}
        </h3>
        <div class="card preset-tonal-surface divide-y divide-surface-200-800">
          {#each group.rows as row (row.id)}
            <FileRow
              {row}
              highlighted={row.id === highlightId}
              error={row.id === errorRowId ? errorMessage : null}
              onDownload={(r) => void download(r)}
              onDelete={askDelete}
            />
          {/each}
        </div>
      </section>
    {/each}
    {#if limitHit}
      <p class="text-xs opacity-60 px-1">{files_limit_note()}</p>
    {/if}
  {/if}
</div>

<ConfirmDialog
  open={deleteTarget !== null}
  title={files_delete_title()}
  description={deleteTarget === null
    ? ""
    : files_delete_body({ name: deleteTarget.displayPath })}
  confirmLabel={common_delete()}
  tone="danger"
  busy={deleting}
  onConfirm={() => void deleteConfirmed()}
  onCancel={() => (deleteTarget = null)}
/>

<Dialog
  open={uploadOpen}
  onOpenChange={(e) => {
    if (!e.open && !uploading) uploadOpen = false;
  }}
>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-sm md:max-w-lg p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {files_upload_title()}
        </Dialog.Title>
        <label class="flex flex-col gap-1 text-sm">
          <span class="opacity-80">{files_upload_name_label()}</span>
          <input class="input text-sm" type="file" onchange={onFileChosen} />
          {#if uploadFile}
            <span class="text-xs opacity-60 truncate">{uploadFile.name}</span>
          {/if}
        </label>
        <label class="flex flex-col gap-1 text-sm">
          <span class="opacity-80">{files_upload_prefix_label()}</span>
          <select class="input text-sm" bind:value={uploadPrefix}>
            {#each uploadAreas() as area (area.value)}
              <option value={area.value}>{area.label}</option>
            {/each}
          </select>
          <span class="text-xs opacity-60">{files_upload_prefix_hint()}</span>
        </label>
        {#if uploadError}
          <p class="text-error-500 dark:text-error-400 text-xs">
            {uploadError}
          </p>
        {/if}
        <div class="flex gap-2">
          <button
            type="button"
            class="btn flex-1 preset-outlined-surface-500"
            disabled={uploading}
            onclick={() => (uploadOpen = false)}
          >
            {common_cancel()}
          </button>
          <button
            type="button"
            class="btn flex-1 preset-filled-primary-500"
            disabled={uploading || uploadFile === null}
            onclick={() => void submitUpload()}
          >
            {uploading ? "…" : files_upload()}
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
