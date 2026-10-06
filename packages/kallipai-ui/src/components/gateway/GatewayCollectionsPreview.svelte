<script lang="ts" module>
  // The tagma profiles page's gateway tab: a browse of the gateway
  // account's collections (own plus the platform catalog) with the
  // tagma's pull selection. Collection edits stay on the gateway
  // page (the edit pointer); the pick names the one collection this
  // tagma pulls, and the read retry is the only other action here.
</script>

<script lang="ts">
  import {
    common_loading,
    common_retry,
    manage_profiles_collection_pull,
    manage_profiles_gateway_collections,
    manage_profiles_gateway_collections_edit,
    manage_profiles_gateway_collections_empty,
    manage_profiles_gateway_collections_readonly,
    user_gateway_platform_catalog,
  } from "../../paraglide/messages.js";
  import type {
    UserCollectionRow,
    UserPlatformCollectionRow,
  } from "../../lib/gateway/client.ts";
  import { collectionRowKey } from "../../lib/manage/profilesGatewayTab.ts";

  let {
    collections,
    platformCatalog,
    loading = false,
    error = null,
    selectedKey = null,
    selectionEditable = false,
    onPickRow = null,
    manageHref,
    onRetry,
  }: {
    /** The gateway account's own collections (null-shape handled by
     * the caller: empty list while the first read runs). */
    collections: readonly UserCollectionRow[];
    /** The platform catalog the account's reach carries. */
    platformCatalog: readonly UserPlatformCollectionRow[];
    /** The store's in-flight marker for the catalog read. */
    loading?: boolean;
    /** The store's failure line for the catalog read. */
    error?: string | null;
    /** The picked row's key (the page owns the key scheme and
     * the dirty state; null = nothing picked). */
    selectedKey?: string | null;
    /** False renders the plain browse (no gateway session, or the
     * daemon not enrolled on the serving platform). */
    selectionEditable?: boolean;
    /** Row-key pick; the key is the row's identity in the page. */
    onPickRow?: ((key: string) => void) | null;
    /** The gateway page itself (the edit-side pointer). */
    manageHref: string;
    onRetry: () => void;
  } = $props();

  // The row-key scheme lives in the shared helper (the page's pick
  // uses the same keys).

  function onPick(row: UserCollectionRow | UserPlatformCollectionRow): void {
    if (selectionEditable && onPickRow !== null) {
      onPickRow(collectionRowKey(row));
    }
  }
</script>

<div class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-3">
  <div class="flex flex-wrap items-center gap-2 justify-between">
    <div class="flex items-center gap-2">
      <p class="font-medium">{manage_profiles_gateway_collections()}</p>
      <span class="badge preset-outlined-surface-500 text-xs shrink-0">
        {manage_profiles_gateway_collections_readonly()}
      </span>
    </div>
    <a class="underline text-xs" href={manageHref}>
      {manage_profiles_gateway_collections_edit()}
    </a>
  </div>

  <!-- The store keeps the previous rows on a failed read, so the error
       line and the loaded rows coexist; the empty copy stays silent
       while an error speaks (a failed first read is not an empty
       account). -->
  {#if error}
    <div class="flex items-center gap-2">
      <p class="text-xs text-error-500" role="alert">{error}</p>
      <button
        type="button"
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        onclick={onRetry}
      >
        {common_retry()}
      </button>
    </div>
  {/if}
  {#if collections.length > 0 || platformCatalog.length > 0}
    <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
      {#each collections as row (row.name)}
        <div class="card preset-tonal-surface p-3 space-y-2">
          <div class="min-w-0 flex items-start gap-2">
            {#if selectionEditable}
              <input
                type="radio"
                name="profiles-pull-collection"
                class="radio shrink-0 mt-0.5"
                checked={selectedKey === collectionRowKey(row)}
                onchange={() => onPick(row)}
                aria-label={manage_profiles_collection_pull({ name: row.name })}
              />
            {/if}
            <div class="min-w-0">
              <span class="text-sm font-medium font-mono truncate">
                {row.name}
              </span>
              {#if row.description}
                <p class="text-xs opacity-60 mt-0.5 truncate">
                  {row.description}
                </p>
              {/if}
            </div>
          </div>
          <div class="flex flex-wrap items-center gap-1.5">
            {#each row.sets as set (set)}
              <span class="badge preset-outlined-surface-500 text-xs font-mono">
                {set}
              </span>
            {/each}
          </div>
        </div>
      {/each}
      {#each platformCatalog as row (row.owner + "/" + row.name)}
        <div class="card preset-tonal-surface p-3 space-y-2">
          <div class="min-w-0 flex items-start gap-2">
            {#if selectionEditable}
              <input
                type="radio"
                name="profiles-pull-collection"
                class="radio shrink-0 mt-0.5"
                checked={selectedKey === collectionRowKey(row)}
                onchange={() => onPick(row)}
                aria-label={manage_profiles_collection_pull({
                  name: row.owner + "/" + row.name,
                })}
              />
            {/if}
            <div class="min-w-0 flex items-center gap-2">
              <span class="text-sm font-medium font-mono truncate">
                {row.name}
              </span>
              <span class="badge preset-outlined-surface-500 text-xs shrink-0">
                {user_gateway_platform_catalog()}
              </span>
            </div>
          </div>
          {#if row.description}
            <p class="text-xs opacity-60 truncate">{row.description}</p>
          {/if}
          <div class="flex flex-wrap items-center gap-1.5">
            {#each row.sets as set (set)}
              <span class="badge preset-outlined-surface-500 text-xs font-mono">
                {set}
              </span>
            {/each}
          </div>
        </div>
      {/each}
    </div>
  {:else if loading}
    <p class="text-xs opacity-60">{common_loading()}</p>
  {:else if !error}
    <p class="text-xs opacity-60">
      {manage_profiles_gateway_collections_empty()}
    </p>
  {/if}
</div>
