<script lang="ts">
  // The gateway admin console (the platform catalog face). The
  // /admin layout's AdminArea renders it only for an online
  // local-admin session (everyone else gets the 403 note there).
  // The console serves two routes: the list view (/admin/gateway)
  // and a collection's detail subface
  // (/admin/gateway/collections/<name>, reached by the `collection`
  // prop); groups, the unassigned panel, and providers render on both, so an
  // unassigned drag always has its drop targets in view -- on the set
  // cards in the detail, or the placement dialog fallback on the
  // list.
  let { collection = null }: { collection?: string | null } = $props();

  import { gatewayStore } from "../../../lib/manage/gateway/store.svelte.ts";
  import PlacementDialog from "../../../components/manage/gateway/PlacementDialog.svelte";
  import {
    isConnected,
    searchAccounts,
  } from "../../../lib/manage/gateway/client.ts";
  import { copyText } from "../../../lib/clipboard.ts";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import { Globe, Pencil, Trash } from "@lucide/svelte";
  import ActionMenu from "../../../components/ActionMenu.svelte";
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  import { memberSetsOf } from "../../../lib/gateway/collections.ts";
  import { adminGatewayPath } from "../../../lib/shell/routes.ts";
  import { navigate } from "../../../lib/shell/port.ts";
  import PageHeader from "../../../components/PageHeader.svelte";
  import ConfirmDialog from "../../../components/ConfirmDialog.svelte";
  import SetsSection from "../../../components/manage/gateway/SetsSection.svelte";
  import CollectionsSection from "../../../components/manage/gateway/CollectionsSection.svelte";
  import SetDialog from "../../../components/manage/gateway/SetDialog.svelte";
  import { createCatalogDetail } from "../../../lib/manage/gateway/catalogDetail.svelte.ts";
  import { createCatalogSetDialogs } from "../../../lib/manage/gateway/catalogSetDialogs.svelte.ts";
  import { createCatalogProviderDialogs } from "../../../lib/manage/gateway/catalogProviderDialogs.svelte.ts";
  import { createCatalogProfileDialogs } from "../../../lib/manage/gateway/catalogProfileDialogs.svelte.ts";
  import { createCatalogCollectionDialogs } from "../../../lib/manage/gateway/catalogCollectionDialogs.svelte.ts";
  import {
    common_loading,
    common_retry,
    common_edit,
    common_remove,
    manage_gateway_collection_actions_aria,
    manage_gateway_collection_not_found,
    manage_gateway_collection_publish,
    manage_gateway_collection_unpublish,
    manage_gateway_reach_everyone,
    manage_gateway_admin_url_missing,
    manage_gateway_heading,
    user_gateway_remove_credential_body,
    manage_gateway_remove_set_body,
    manage_gateway_remove_provider_body,
    manage_gateway_remove_unassigned_body,
    manage_gateway_remove_group_body,
    manage_gateway_providers,
    manage_gateway_subtitle,
    manage_gateway_title,
  } from "../../../paraglide/messages.js";
  import ProvidersSection from "../../../components/manage/gateway/ProvidersSection.svelte";
  import ProviderDialog from "../../../components/manage/gateway/ProviderDialog.svelte";
  import UnassignedSection from "../../../components/manage/gateway/UnassignedSection.svelte";
  import ProfileDialog from "../../../components/manage/gateway/ProfileDialog.svelte";
  import ProviderCredentialDialog from "../../../components/manage/gateway/ProviderCredentialDialog.svelte";
  import SetCreateDialog from "../../../components/manage/gateway/SetCreateDialog.svelte";
  import CollectionDialog from "../../../components/manage/gateway/CollectionDialog.svelte";
  import GroupsSection from "../../../components/manage/gateway/GroupsSection.svelte";
  import GroupDialog from "../../../components/manage/gateway/GroupDialog.svelte";

  // One-shot read-side load once a connection is known (the
  // store's isLoading dedupes concurrent calls).
  let initialLoadDone = $state(false);
  $effect(() => {
    if (isConnected() && !initialLoadDone) {
      initialLoadDone = true;
      void gatewayStore.refreshAll();
    }
  });

  // The open collection's detail row (null on the list route, or
  // while the first read is still in flight).
  const detailRow = $derived(
    collection !== null
      ? (gatewayStore.collections?.find((c) => c.name === collection) ?? null)
      : null,
  );
  // The detail's member sets: the collection row carries the member
  // names, the store's flat set list carries the rows.
  const detailMemberSets = $derived.by(() => {
    if (!detailRow) return [];
    return memberSetsOf(gatewayStore.sets ?? [], detailRow);
  });
  // The detail's unassigned panel, read through its own controller: the
  // request token and the "which collection do these rows belong to"
  // bookkeeping live beside the rows they guard.
  const detail = createCatalogDetail({
    collection: () => detailRow?.name ?? null,
    readUnassigned: (name) => gatewayStore.unassignedIn(name),
  });
  // The re-read keys on the collection name, never on the row object:
  // the store swaps the row out (refreshAll) without the rows
  // changing, so identity is no witness. Every mutation reloads
  // explicitly for the same reason (save, delete, drop).
  $effect(() => {
    // Re-read whenever the open collection changes.
    void detailRow?.name;
    void detail.refresh();
  });

  // The dialog controllers: each group owns its target, its busy latch,
  // the server's reason and its guards. The page keeps the read side, the
  // template, and this wiring. A cancel arrives as the controller's method,
  // not as a closure: the dialogs hand their close event over, and a cancel
  // takes no argument.
  const setDialogs = createCatalogSetDialogs({
    store: gatewayStore,
    collection: () => detailRow?.name ?? null,
    reloadUnassigned: () => detail.refresh(),
  });
  const providerDialogs = createCatalogProviderDialogs({ store: gatewayStore });
  const profileDialogs = createCatalogProfileDialogs({
    store: gatewayStore,
    reloadUnassigned: () => detail.refresh(),
  });
  const collectionDialogs = createCatalogCollectionDialogs({
    store: gatewayStore,
    // The detail's subject is gone once the delete lands; the list is
    // where the console goes.
    afterRemove: () => navigate(adminGatewayPath()),
  });

  const groupName = (groupId: string): string =>
    (gatewayStore.groups ?? []).find((g) => g.group_id === groupId)?.name ??
    groupId;
</script>

<svelte:head><title>{manage_gateway_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <PageHeader>
    {#snippet title()}
      <h1 class="text-sm font-semibold truncate">
        {manage_gateway_heading()}
      </h1>
      <p class="text-xs opacity-50 truncate">{manage_gateway_subtitle()}</p>
    {/snippet}
  </PageHeader>

  <div class="px-2 md:p-6 max-w-3xl space-y-6">
    {#if !isConnected()}
      <div class="card preset-tonal-surface p-4">
        <p class="text-sm opacity-70">{manage_gateway_admin_url_missing()}</p>
      </div>
    {:else}
      {#if gatewayStore.error}
        <p class="text-sm text-error-500" role="alert">
          {gatewayStore.error}
        </p>
      {/if}

      {#if detailRow}
        <section class="card preset-tonal-surface p-4 space-y-3">
          <div class="flex items-start justify-between gap-2">
            <div class="min-w-0">
              <h2 class="text-sm font-semibold font-mono truncate">
                {detailRow.name}
              </h2>
              {#if detailRow.description}
                <p class="text-xs opacity-60 mt-0.5 truncate">
                  {detailRow.description}
                </p>
              {/if}
            </div>
            <ActionMenu
              label={manage_gateway_collection_actions_aria()}
              contentClass="card preset-tonal-surface p-1 min-w-[10rem] max-h-72 overflow-y-auto"
              onSelect={(value) =>
                collectionDialogs.collection.applyAction(detailRow, value)}
            >
              {#each gatewayStore.groups ?? [] as group (group.group_id)}
                {#if !detailRow.publications.includes(group.group_id)}
                  <Menu.Item value="publish:{group.group_id}" class={MENU_ITEM}>
                    <Globe class="size-4" />
                    {manage_gateway_collection_publish()}
                    <span class="font-mono opacity-60">
                      {group.group_id === "everyone"
                        ? manage_gateway_reach_everyone()
                        : group.name}
                    </span>
                  </Menu.Item>
                {/if}
              {/each}
              {#each detailRow.publications as groupId (groupId)}
                <Menu.Item value="unpublish:{groupId}" class={MENU_ITEM_DANGER}>
                  {manage_gateway_collection_unpublish()}
                  <span class="font-mono opacity-60">
                    {groupId === "everyone"
                      ? manage_gateway_reach_everyone()
                      : groupName(groupId)}
                  </span>
                </Menu.Item>
              {/each}
              <Menu.Item value="edit" class={MENU_ITEM}>
                <Pencil class="size-4" />
                {common_edit()}
              </Menu.Item>
              <Menu.Item value="remove" class={MENU_ITEM_DANGER}>
                <Trash class="size-4" />
                {common_remove()}
              </Menu.Item>
            </ActionMenu>
          </div>
        </section>
        <SetsSection
          sets={detailMemberSets}
          defaultSet={detailRow.default_set}
          onEdit={(set) => void setDialogs.edit.edit(set)}
          onRemove={(set) => setDialogs.remove.ask(set)}
          onAdd={() => setDialogs.create.show()}
          onSetDefault={(set) =>
            void gatewayStore.setCollectionDefault(detailRow.name, set.name)}
          dragOverSet={profileDialogs.placement.dragOverSet}
          onSetDragOver={(name) => profileDialogs.placement.dragOver(name)}
          onSetDragLeave={(name) => profileDialogs.placement.dragLeave(name)}
          onSetDrop={(name) => void profileDialogs.placement.dropToSet(name)}
        />
      {:else if collection !== null}
        {#if gatewayStore.collections !== null}
          <div class="card preset-tonal-surface p-4 space-y-2">
            <p class="text-sm opacity-70">
              {manage_gateway_collection_not_found({ name: collection })}
            </p>
          </div>
        {:else if gatewayStore.error !== null}
          <div class="card preset-tonal-surface p-4 space-y-2">
            <p class="text-sm text-error-500" role="alert">
              {gatewayStore.error}
            </p>
            <button
              type="button"
              class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
              onclick={() => void gatewayStore.refreshAll()}
            >
              {common_retry()}
            </button>
          </div>
        {:else}
          <p class="text-sm opacity-60">{common_loading()}</p>
        {/if}
      {:else}
        <div
          role="region"
          ondragover={(e) => e.preventDefault()}
          ondrop={(e) => profileDialogs.placement.dropAnywhere(e)}
        >
          <CollectionsSection
            collections={gatewayStore.collections ?? []}
            groups={gatewayStore.groups ?? []}
            onAdd={() => collectionDialogs.collection.create()}
          />
        </div>
      {/if}

      <GroupsSection
        groups={gatewayStore.groups ?? []}
        onEdit={(row) => collectionDialogs.group.show(row)}
        onRemove={(row) => collectionDialogs.group.remove.ask(row)}
        onAdd={() => collectionDialogs.group.create()}
      />
      <UnassignedSection
        unassigned={detailRow
          ? (detail.rows ?? [])
          : (gatewayStore.unassigned ?? [])}
        scope={detailRow?.name ?? null}
        onCreate={() => profileDialogs.unassigned.create()}
        onEdit={(row) => profileDialogs.unassigned.show(row)}
        onDelete={(row) => profileDialogs.unassigned.remove.ask(row)}
        onDragStart={(profile) => profileDialogs.placement.startDrag(profile)}
        onDragEnd={profileDialogs.placement.clearDrag}
      />

      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_gateway_providers()}
        </h2>
        <ProvidersSection
          providers={gatewayStore.providers ?? []}
          onEdit={(row) => providerDialogs.provider.show(row)}
          onRemove={(row) => providerDialogs.provider.remove.ask(row)}
          onCreate={() => providerDialogs.provider.create()}
          onCredential={(row) => providerDialogs.credential.show(row)}
        />
      </section>
    {/if}
  </div>
</div>

<ConfirmDialog
  open={setDialogs.remove.open}
  title={setDialogs.remove.target?.name ?? ""}
  description={manage_gateway_remove_set_body({
    name: setDialogs.remove.target?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={setDialogs.remove.busy}
  error={setDialogs.remove.error}
  onConfirm={() => void setDialogs.remove.confirm()}
  onCancel={setDialogs.remove.cancel}
/>

<SetDialog
  open={setDialogs.edit.open}
  set={setDialogs.edit.row}
  busy={setDialogs.edit.busy}
  error={setDialogs.edit.error}
  landed={setDialogs.edit.landed}
  onSave={(result) => void setDialogs.edit.save(result)}
  onCancel={setDialogs.edit.cancel}
/>

<SetCreateDialog
  open={setDialogs.create.open}
  names={setDialogs.create.names}
  busy={setDialogs.create.busy}
  error={setDialogs.create.error}
  onSave={(result) => void setDialogs.create.save(result)}
  onCancel={setDialogs.create.cancel}
/>

<ConfirmDialog
  open={providerDialogs.provider.remove.open}
  title={providerDialogs.provider.remove.target ?? ""}
  description={manage_gateway_remove_provider_body({
    name: providerDialogs.provider.remove.target ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={providerDialogs.provider.remove.busy}
  error={providerDialogs.provider.remove.error}
  onConfirm={() => void providerDialogs.provider.remove.confirm()}
  onCancel={providerDialogs.provider.remove.cancel}
/>

<ProviderDialog
  open={providerDialogs.provider.open}
  row={providerDialogs.provider.row}
  busy={providerDialogs.provider.busy}
  error={providerDialogs.provider.error}
  onSave={(result) => void providerDialogs.provider.save(result)}
  onCancel={providerDialogs.provider.cancel}
/>

<ProfileDialog
  open={profileDialogs.unassigned.open}
  providers={gatewayStore.providers ?? []}
  collections={gatewayStore.collections ?? []}
  pinnedCollection={detailRow?.name ?? null}
  row={profileDialogs.unassigned.row}
  busy={profileDialogs.unassigned.busy}
  error={profileDialogs.unassigned.error}
  onSave={(result) => void profileDialogs.unassigned.save(result)}
  onCancel={profileDialogs.unassigned.cancel}
/>

<ConfirmDialog
  open={profileDialogs.unassigned.remove.open}
  title={profileDialogs.unassigned.remove.target?.profile_id ?? ""}
  description={manage_gateway_remove_unassigned_body({
    name: profileDialogs.unassigned.remove.target?.profile_id ?? "",
    collection: profileDialogs.unassigned.remove.target?.collection_name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={profileDialogs.unassigned.remove.busy}
  error={profileDialogs.unassigned.remove.error}
  onConfirm={() => void profileDialogs.unassigned.remove.confirm()}
  onCancel={profileDialogs.unassigned.remove.cancel}
/>

<ProviderCredentialDialog
  open={providerDialogs.credential.open}
  row={providerDialogs.credential.row}
  busy={providerDialogs.credential.busy}
  error={providerDialogs.credential.error}
  onSave={(result) => void providerDialogs.credential.save(result)}
  onRemove={providerDialogs.credential.remove.ask}
  onCancel={providerDialogs.credential.cancel}
/>

<ConfirmDialog
  open={providerDialogs.credential.remove.open}
  title={providerDialogs.credential.remove.target ?? ""}
  description={user_gateway_remove_credential_body({
    id: providerDialogs.credential.remove.target ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={providerDialogs.credential.remove.busy}
  error={providerDialogs.credential.remove.error}
  onConfirm={() => void providerDialogs.credential.remove.confirm()}
  onCancel={providerDialogs.credential.remove.cancel}
/>

<CollectionDialog
  open={collectionDialogs.collection.open}
  collection={collectionDialogs.collection.row}
  existingNames={(gatewayStore.collections ?? []).map((c) => c.name)}
  busy={collectionDialogs.collection.busy}
  error={collectionDialogs.collection.error}
  onSave={(result) => void collectionDialogs.collection.save(result)}
  onCancel={collectionDialogs.collection.cancel}
/>

<GroupDialog
  open={collectionDialogs.group.open}
  group={collectionDialogs.group.row}
  busy={collectionDialogs.group.busy || collectionDialogs.group.memberBusy}
  error={collectionDialogs.group.error}
  onSave={(result) => void collectionDialogs.group.save(result)}
  onAddMember={(account) => void collectionDialogs.group.addMember(account)}
  onRemoveMember={(account) =>
    void collectionDialogs.group.removeMember(account)}
  onSearch={(query) => searchAccounts(query)}
  onCancel={collectionDialogs.group.cancel}
/>

<ConfirmDialog
  open={collectionDialogs.group.remove.open}
  title={collectionDialogs.group.remove.target?.name ?? ""}
  description={manage_gateway_remove_group_body({
    name: collectionDialogs.group.remove.target?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={collectionDialogs.group.remove.busy}
  error={collectionDialogs.group.remove.error}
  onConfirm={() => void collectionDialogs.group.remove.confirm()}
  onCancel={collectionDialogs.group.remove.cancel}
/>

<ConfirmDialog
  open={collectionDialogs.collection.remove.open}
  title={collectionDialogs.collection.remove.target?.name ?? ""}
  description={collectionDialogs.collection.remove.body}
  confirmLabel={common_remove()}
  tone="danger"
  busy={collectionDialogs.collection.remove.busy}
  error={collectionDialogs.collection.remove.error}
  onConfirm={() => void collectionDialogs.collection.remove.confirm()}
  onCancel={collectionDialogs.collection.remove.cancel}
/>

<PlacementDialog
  open={profileDialogs.placement.open}
  profileId={profileDialogs.placement.row?.profile_id ?? null}
  sourceCollection={profileDialogs.placement.row?.collection_name ?? undefined}
  collections={gatewayStore.collections ?? []}
  sets={gatewayStore.sets ?? []}
  busy={profileDialogs.placement.busy}
  error={gatewayStore.error}
  onConfirm={(setName) => void profileDialogs.placement.confirm(setName)}
  onCancel={profileDialogs.placement.cancel}
/>
