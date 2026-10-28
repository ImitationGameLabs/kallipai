<script lang="ts">
  // The platform reach section: one navigation card per catalog
  // collection (name / description / set and profile counts / live
  // audiences). The card is an anchor into the collection's detail
  // subface; every mutation (edit, remove, publish) lives there. The
  // header row carries the create trigger (the CollectionDialog).
  import { adminGatewayCollectionPath } from "../../../lib/shell/routes.ts";
  import type { AdminCollectionRow } from "../../../lib/manage/gateway/client.ts";
  import {
    manage_gateway_collection_add,
    manage_gateway_collection_profiles_count,
    manage_gateway_collection_reach,
    manage_gateway_collection_sets_count,
    manage_gateway_collections,
    manage_gateway_reach_everyone,
  } from "../../../paraglide/messages.js";

  let {
    collections,
    groups,
    onAdd,
  }: {
    /** The catalog's collections (empty while the first read runs). */
    collections: readonly AdminCollectionRow[];
    /** The platform groups: the audience names behind the ids. */
    groups: readonly { group_id: string; name: string }[];
    onAdd: () => void;
  } = $props();

  const groupName = (groupId: string): string =>
    groups.find((g) => g.group_id === groupId)?.name ?? groupId;
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_gateway_collections()}
    </h2>
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={onAdd}
    >
      {manage_gateway_collection_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each collections as collection (collection.name)}
      <a
        href={adminGatewayCollectionPath(collection.name)}
        class="card preset-tonal-surface p-4 space-y-2 block"
      >
        <div class="min-w-0">
          <span class="text-sm font-medium font-mono truncate">
            {collection.name}
          </span>
          {#if collection.description}
            <p class="text-xs opacity-60 mt-0.5 truncate">
              {collection.description}
            </p>
          {/if}
        </div>
        <div class="flex items-center gap-1.5">
          <span class="text-xs opacity-60">
            {manage_gateway_collection_sets_count({
              count: collection.set_count,
            })}
          </span>
          <span class="text-xs opacity-40">·</span>
          <span class="text-xs opacity-60">
            {manage_gateway_collection_profiles_count({
              count: collection.profile_count,
            })}
          </span>
        </div>
        <div class="flex items-center gap-1.5 flex-wrap">
          <span class="text-xs opacity-60">
            {manage_gateway_collection_reach()}:
          </span>
          {#if collection.publications.length === 0}
            <span class="text-xs font-mono opacity-40">--</span>
          {:else}
            {#each collection.publications as groupId (groupId)}
              <span class="badge preset-outlined-surface-500 text-xs">
                {groupId === "everyone"
                  ? manage_gateway_reach_everyone()
                  : groupName(groupId)}
              </span>
            {/each}
          {/if}
        </div>
      </a>
    {/each}
  </div>
</section>
