<script lang="ts">
  // The platform catalog section: one read-only card per published
  // platform collection the caller's reach carries, with its sets as
  // chips. No actions: the catalog is platform material surfaced for
  // browsing. The page hides the whole section when the list is empty.
  import type { UserPlatformCollectionRow } from "../../lib/gateway/client.ts";
  import {
    user_gateway_platform_catalog,
    user_gateway_sets,
  } from "../../paraglide/messages.js";

  let {
    collections,
  }: {
    /** The platform collections on the caller's reach. */
    collections: readonly UserPlatformCollectionRow[];
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {user_gateway_platform_catalog()}
  </h2>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each collections as row (row.owner + "/" + row.name)}
      <div class="card preset-tonal-surface p-4 space-y-2">
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
        <div class="flex flex-wrap items-center gap-1.5">
          <span class="text-xs opacity-60">{user_gateway_sets()}</span>
          {#each row.sets as set (set)}
            <span class="badge preset-outlined-surface-500 text-xs font-mono">
              {set}
            </span>
          {/each}
        </div>
      </div>
    {/each}
  </div>
</section>
