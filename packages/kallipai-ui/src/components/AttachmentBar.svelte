<script lang="ts">
  import { File, LoaderCircle, RotateCcw, X } from "@lucide/svelte";
  import {
    chat_attachment_failed,
    chat_attachment_retry,
    chat_attachment_too_large,
    chat_attachment_uploading,
    chat_attachments_aria,
    chat_attachment_remove,
    chat_file_size_bytes,
  } from "../paraglide/messages.js";
  import type { AttachmentItem } from "../lib/attachments";

  let {
    items,
    onRetry,
    onRemove,
  }: {
    items: AttachmentItem[];
    onRetry: (id: string) => void;
    onRemove: (id: string) => void;
  } = $props();
</script>

{#if items.length > 0}
  <!-- Below the input card, above the notice row: one row per picked file,
       so multi-select stays scannable and each item carries its own state
       (the composer spec's per-item status machine). -->
  <ul class="mt-1.5 space-y-1" aria-label={chat_attachments_aria()}>
    {#each items as item (item.id)}
      <li
        class="flex items-center gap-2 rounded-xl border border-surface-300-700 px-2.5 py-1.5 text-sm"
      >
        <File class="size-4 shrink-0 opacity-70" aria-hidden="true" />
        <span class="min-w-0 flex-1 truncate">{item.name}</span>
        {#if item.status === "uploading"}
          <span class="flex items-center gap-1 text-xs opacity-60">
            <LoaderCircle class="size-3.5 animate-spin" aria-hidden="true" />
            {chat_attachment_uploading({ name: item.name })}
          </span>
        {:else if item.status === "ready"}
          <span class="text-xs opacity-60"
            >{chat_file_size_bytes({ size: item.size })}</span
          >
        {:else if item.status === "too_large"}
          <span class="text-xs text-error-500">
            {chat_attachment_too_large({ name: item.name, max: "100 MB" })}
          </span>
        {:else if item.status === "failed"}
          <span class="text-xs text-error-500">
            {chat_attachment_failed({ name: item.name })}
          </span>
          <button
            type="button"
            class="flex items-center gap-1 text-xs text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
            onclick={() => onRetry(item.id)}
          >
            <RotateCcw class="size-3.5" aria-hidden="true" />
            {chat_attachment_retry()}
          </button>
        {/if}
        <button
          type="button"
          class="shrink-0 opacity-60 hover:opacity-100 cursor-pointer"
          aria-label={chat_attachment_remove({ name: item.name })}
          onclick={() => onRemove(item.id)}
        >
          <X class="size-4" aria-hidden="true" />
        </button>
      </li>
    {/each}
  </ul>
{/if}
