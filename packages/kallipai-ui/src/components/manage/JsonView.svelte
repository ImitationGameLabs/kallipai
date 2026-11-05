<script lang="ts">
  // One node of a parsed tree. The component renders itself for children, so
  // the depth of the markup follows the depth of the data and no level needs
  // its own branch.
  //
  // The two bits of state it does not own -- which containers are open and
  // which long strings are unfolded -- are passed in from above, keyed by the
  // node's path. That is what lets a reader switch between the tree and the
  // stored text and come back to the branches they opened.
  import { ChevronRight } from "@lucide/svelte";
  import JsonView from "./JsonView.svelte";
  import {
    childPath,
    foldsAt,
    foldString,
    nodeSize,
    previewOf,
    type JsonNode,
  } from "../../lib/manage/jsonView.ts";
  import {
    manage_history_container_fields,
    manage_history_container_items,
    manage_history_show_all,
    manage_history_show_less,
  } from "../../paraglide/messages.js";

  let {
    node,
    label,
    path,
    depth = 0,
    expanded,
    unfolded,
    onToggle,
    onUnfold,
  }: {
    node: JsonNode;
    /** What sits left of the value: a key, an index, or nothing at the root. */
    label: string;
    /** Where this node sits in the tree, so its fold state follows the data. */
    path: string;
    depth?: number;
    expanded: Record<string, boolean>;
    unfolded: Record<string, boolean>;
    onToggle: (path: string, next: boolean) => void;
    onUnfold: (path: string, next: boolean) => void;
  } = $props();

  /** Two levels open by default: enough to see the shape of an answer
   * without scrolling through it. */
  const DEFAULT_OPEN_DEPTH = 2;

  /** The deepest level that still adds indent; past it the tree would be a
   * column of slivers, so depth is carried by the fold controls alone. */
  const MAX_INDENT_DEPTH = 6;

  const open = $derived(expanded[path] ?? depth < DEFAULT_OPEN_DEPTH);
  const size = $derived(nodeSize(node));
  const preview = $derived(open ? "" : previewOf(node));
  const countText = $derived(
    node.kind === "array"
      ? manage_history_container_items({ count: size })
      : manage_history_container_fields({ count: size }),
  );
  const unfoldedHere = $derived(unfolded[path] === true);
  const foldable = $derived(node.kind === "string" && foldsAt(node.value));
</script>

{#if node.kind === "object" || node.kind === "array"}
  <div class="py-0.5">
    <button
      type="button"
      class="flex w-full items-start gap-1 text-left font-mono"
      aria-expanded={open}
      onclick={() => onToggle(path, !open)}
    >
      <ChevronRight
        size={12}
        class="mt-1.5 shrink-0 transition-transform {open ? 'rotate-90' : ''}"
      />
      {#if label !== ""}
        <span class="font-medium text-surface-600 dark:text-surface-300"
          >{label}:</span
        >
      {/if}
      <span class="opacity-60">{countText}</span>
      {#if preview !== ""}
        <span class="truncate opacity-50">{preview}</span>
      {/if}
    </button>
    {#if open}
      <div
        class="border-l border-surface-200-800 {depth < MAX_INDENT_DEPTH
          ? 'pl-5'
          : 'pl-1'}"
      >
        {#if node.kind === "object"}
          {#each node.entries as entry (entry.key)}
            <JsonView
              node={entry.node}
              label={entry.key}
              path={childPath(path, entry.key)}
              depth={depth + 1}
              {expanded}
              {unfolded}
              {onToggle}
              {onUnfold}
            />
          {/each}
        {:else}
          {#each node.items as item, index (index)}
            <JsonView
              node={item}
              label={String(index)}
              path={childPath(path, index)}
              depth={depth + 1}
              {expanded}
              {unfolded}
              {onToggle}
              {onUnfold}
            />
          {/each}
        {/if}
      </div>
    {/if}
  </div>
{:else}
  <div class="flex flex-wrap items-baseline gap-x-1 py-0.5 font-mono">
    {#if label !== ""}
      <span class="font-medium text-surface-600 dark:text-surface-300"
        >{label}:</span
      >
    {/if}
    {#if node.kind === "string"}
      <span class="whitespace-pre-wrap break-words"
        >{foldable && !unfoldedHere ? foldString(node.value) : node.value}</span
      >
      {#if foldable}
        <button
          type="button"
          class="text-xs text-primary-600 hover:underline dark:text-primary-400"
          onclick={() => onUnfold(path, !unfoldedHere)}
          aria-expanded={unfoldedHere}
        >
          {unfoldedHere
            ? manage_history_show_less()
            : manage_history_show_all()}
        </button>
      {/if}
    {:else if node.kind === "number"}
      <span class="tabular-nums">{node.text}</span>
    {:else if node.kind === "boolean"}
      <span class="rounded bg-surface-100-900 px-1"
        >{node.value ? "true" : "false"}</span
      >
    {:else}
      <span class="italic opacity-50">null</span>
    {/if}
  </div>
{/if}
