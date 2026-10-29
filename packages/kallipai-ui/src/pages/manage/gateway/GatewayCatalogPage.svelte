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
  import type {
    AdminCollectionRow,
    AdminProfileRow,
    AdminSetRow,
    GroupRow,
    ProviderRow,
  } from "../../../lib/manage/gateway/client.ts";
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
    manage_gateway_remove_collection_body,
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
  // The detail's unassigned: read for the open collection, so the panel can
  // never offer a profile whose sets live elsewhere.
  let detailUnassigned = $state<AdminProfileRow[] | null>(null);
  let detailUnassignedRequest = 0;
  // Which collection the panel's rows belong to (plain state: the
  // effect must not track its own bookkeeping and re-read twice).
  let detailUnassignedFor: string | null = null;
  async function reloadDetailUnassigned(): Promise<void> {
    const request = ++detailUnassignedRequest;
    const name = detailRow?.name ?? null;
    if (name !== detailUnassignedFor) {
      // A switch clears first: a slow or failed read must not leave
      // the previous collection's rows under the new detail.
      detailUnassigned = null;
      detailUnassignedFor = name;
    }
    if (name === null) return;
    const rows = await gatewayStore.unassignedIn(name);
    // Only the newest read may land; a late answer for a collection
    // that already left would otherwise repaint it.
    if (rows !== undefined && request === detailUnassignedRequest) {
      detailUnassigned = rows;
    }
  }
  // The re-read keys on the collection name, never on the row object:
  // the store swaps the row out (refreshAll) without the rows
  // changing, so identity is no witness. Every mutation reloads
  // explicitly for the same reason (save, delete, drop).
  $effect(() => {
    // Re-read whenever the open collection changes.
    void detailRow?.name;
    void reloadDetailUnassigned();
  });
  const groupName = (groupId: string): string =>
    (gatewayStore.groups ?? []).find((g) => g.group_id === groupId)?.name ??
    groupId;

  let editSet = $state<AdminSetRow | null>(null);
  let dialogOpen = $state(false);
  let editRequest = $state(0);
  let removeTarget = $state<AdminSetRow | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);
  let credentialTarget = $state<{
    provider_id: string;
    base_url: string | null;
  } | null>(null);
  let credentialBusy = $state(false);
  let credentialError = $state<string | null>(null);
  let credentialRemoveTarget = $state<string | null>(null);
  let credentialRemoveBusy = $state(false);
  let credentialRemoveError = $state<string | null>(null);
  // The create-set dialog's wiring (the dialog owns the draft).
  let setCreateOpen = $state(false);
  let setCreateBusy = $state(false);
  let setCreateError = $state<string | null>(null);
  // Unassigned-to-set drop wiring: the page owns the drag state (the
  // sections report the raw lifecycle), and a busy latch keeps a
  // second drop from racing the in-flight member update.
  let dragProfileId = $state<string | null>(null);
  // The dragged row's collection: an unassigned row can only join a set
  // of its own collection, so the drop names the one collection the
  // land in.
  let dragSourceCollection = $state<string | null>(null);
  let dragOverSet = $state<string | null>(null);
  let dropBusy = $state(false);
  // Each edit click takes a request token; only the newest token's
  // response may seed the dialog. A slow loadSet answering after a
  // later click (or after the dialog already opened) would otherwise
  // swap the edit target under the open dialog, and a save would PATCH
  // one set's members onto another's registry entry.

  async function onEditSet(set: AdminSetRow): Promise<void> {
    const request = ++editRequest;
    const row = await gatewayStore.loadSet(set.collection_name, set.name);
    if (row && request === editRequest) {
      editSet = row;
      dialogOpen = true;
    }
  }

  async function onSaveSet(result: {
    description: string;
    members: string[];
  }): Promise<void> {
    const target = editSet;
    dialogOpen = false;
    editSet = null;
    if (!target) return;
    await gatewayStore.saveSet(target.collection_name, target.name, {
      description: result.description,
      members: result.members,
    });
    // Dropping a set's last member flips that profile back to unassigned
    // server-side, so a member edit moves the unassigned panel too.
    if (gatewayStore.error === null) await reloadDetailUnassigned();
  }

  // The provider pool's dialog state. editProvider null = the create
  // form; the dialog error surfaces the store's reason (a referenced
  // provider refuses to delete with a 409, an id collision too).
  let editProvider = $state<{
    provider_id: string;
    family: string;
    base_url: string | null;
  } | null>(null);
  let providerDialogOpen = $state(false);
  let providerBusy = $state(false);
  let providerError = $state<string | null>(null);
  let removeProviderTarget = $state<string | null>(null);
  let editUnassigned = $state<AdminProfileRow | null>(null);
  let unassignedDialogOpen = $state(false);
  let unassignedBusy = $state(false);
  let unassignedError = $state<string | null>(null);
  let removeUnassignedTarget = $state<AdminProfileRow | null>(null);

  function openProviderDialog(
    row: {
      provider_id: string;
      family: string;
      base_url: string | null;
    } | null,
  ): void {
    editProvider = row;
    providerError = null;
    providerDialogOpen = true;
  }

  async function saveProvider(result: {
    provider_id: string;
    family: string;
    base_url: string | null;
    api_key: string | null;
  }): Promise<void> {
    providerBusy = true;
    providerError = null;
    if (editProvider === null) {
      const ok = await gatewayStore.addProvider(result);
      providerBusy = false;
      if (!ok) {
        providerError = gatewayStore.error;
        return;
      }
    } else {
      const ok = await gatewayStore.saveProvider(editProvider.provider_id, {
        family: result.family,
        base_url: result.base_url,
        api_key: result.api_key,
      });
      providerBusy = false;
      if (!ok) {
        providerError = gatewayStore.error;
        return;
      }
    }
    providerDialogOpen = false;
    editProvider = null;
  }

  async function confirmRemoveProvider(): Promise<void> {
    const id = removeProviderTarget;
    if (!id) return;
    providerBusy = true;
    providerError = null;
    await gatewayStore.removeProvider(id);
    providerBusy = false;
    if (gatewayStore.error !== null) {
      // Keep the dialog open and surface the reason (a referenced
      // provider refuses to delete with a 409).
      providerError = gatewayStore.error;
    } else {
      removeProviderTarget = null;
    }
  }

  function openUnassignedDialog(row: AdminProfileRow | null): void {
    editUnassigned = row;
    unassignedError = null;
    unassignedDialogOpen = true;
  }

  async function saveUnassigned(result: {
    collection: string;
    profile_id: string;
    provider_id: string;
    model: string;
    max_context_window: number | null;
    effort: string | null;
    modalities: string[] | null;
    max_budget: number | null;
    tpm_limit: number | null;
    rpm_limit: number | null;
  }): Promise<void> {
    unassignedBusy = true;
    unassignedError = null;
    if (editUnassigned === null) {
      const ok = await gatewayStore.addUnassigned(result.collection, {
        profile_id: result.profile_id,
        provider_id: result.provider_id,
        model: result.model,
        max_context_window: result.max_context_window,
        effort: result.effort,
        modalities: result.modalities,
      });
      unassignedBusy = false;
      if (!ok) {
        unassignedError = gatewayStore.error;
        return;
      }
    } else {
      const ok = await gatewayStore.saveUnassigned(
        editUnassigned.collection_name,
        editUnassigned.profile_id,
        {
          model: result.model,
          max_context_window: result.max_context_window,
          effort: result.effort,
          modalities: result.modalities,
        },
      );
      unassignedBusy = false;
      if (!ok) {
        unassignedError = gatewayStore.error;
        return;
      }
    }
    await reloadDetailUnassigned();
    unassignedDialogOpen = false;
    editUnassigned = null;
  }

  async function confirmRemoveUnassigned(): Promise<void> {
    const row = removeUnassignedTarget;
    if (row === null) return;
    unassignedBusy = true;
    unassignedError = null;
    await gatewayStore.removeUnassigned(row.collection_name, row.profile_id);
    unassignedBusy = false;
    if (gatewayStore.error !== null) {
      unassignedError = gatewayStore.error;
    } else {
      await reloadDetailUnassigned();
      removeUnassignedTarget = null;
    }
  }

  async function confirmRemove(): Promise<void> {
    const target = removeTarget;
    if (!target) return;
    removeBusy = true;
    removeError = null;
    await gatewayStore.removeSet(target.collection_name, target.name);
    removeBusy = false;
    if (gatewayStore.error !== null) {
      // Keep the dialog open and surface the reason (a referenced set
      // refuses to delete with a 409).
      removeError = gatewayStore.error;
    } else {
      removeTarget = null;
      await reloadDetailUnassigned();
    }
  }

  // The collection dialog's wiring: edit carries the list row (the
  // list view already holds the full shape), create opens the blank
  // form; save branches on which shape opened it.
  let editCollection = $state<AdminCollectionRow | null>(null);
  let collectionDialogOpen = $state(false);
  let collectionBusy = $state(false);
  let collectionError = $state<string | null>(null);
  let removeCollectionTarget = $state<AdminCollectionRow | null>(null);
  // The delete confirm's copy, built from the row: both counts are the
  // server's numbers, so the dialog quotes what the cascade will take.
  const removeCollectionBody = $derived(
    removeCollectionTarget === null
      ? ""
      : manage_gateway_remove_collection_body({
          name: removeCollectionTarget.name,
          sets: removeCollectionTarget.set_count,
          profiles: removeCollectionTarget.profile_count,
        }),
  );
  let removeCollectionBusy = $state(false);
  let removeCollectionError = $state<string | null>(null);

  function openCollectionDialog(row: AdminCollectionRow | null): void {
    editCollection = row;
    collectionError = null;
    collectionDialogOpen = true;
  }

  async function saveCollection(result: {
    name: string;
    description: string;
  }): Promise<void> {
    collectionBusy = true;
    collectionError = null;
    const target = editCollection?.name ?? null;
    const ok =
      target !== null
        ? await gatewayStore.saveCollection(target, {
            description: result.description,
          })
        : await gatewayStore.addCollection(result);
    collectionBusy = false;
    if (!ok) {
      // The PATCH was refused; keep the dialog open and surface the
      // reason (a name collision answers with a 409). A refused
      // refresh does not land here: it rides the page-level alert.
      collectionError = gatewayStore.error;
      return;
    }
    collectionDialogOpen = false;
    editCollection = null;
  }

  async function confirmRemoveCollection(): Promise<void> {
    const name = removeCollectionTarget?.name;
    if (!name) return;
    removeCollectionBusy = true;
    removeCollectionError = null;
    await gatewayStore.removeCollection(name);
    removeCollectionBusy = false;
    if (gatewayStore.error !== null) {
      removeCollectionError = gatewayStore.error;
    } else {
      removeCollectionTarget = null;
      // The detail's subject is gone; the detail route with it.
      void navigate(adminGatewayPath());
    }
  }

  let editGroup = $state<GroupRow | null>(null);
  let groupDialogOpen = $state(false);
  let groupBusy = $state(false);
  let memberBusy = $state(false);
  let groupError = $state<string | null>(null);
  let removeGroupTarget = $state<GroupRow | null>(null);
  let removeGroupBusy = $state(false);
  let removeGroupError = $state<string | null>(null);
  // The dialog renders the live row: member ops re-read the whole
  // list on each landing and the store swaps the array, so resolve
  // by id instead of holding the row captured at the click.
  const editGroupLive = $derived.by(() => {
    if (editGroup === null) return null;
    const id = editGroup.group_id;
    return gatewayStore.groups?.find((g) => g.group_id === id) ?? editGroup;
  });

  function openGroupDialog(row: GroupRow | null): void {
    editGroup = row;
    groupError = null;
    groupDialogOpen = true;
  }

  async function saveGroup(result: { name: string }): Promise<void> {
    groupBusy = true;
    groupError = null;
    const target = editGroup?.group_id ?? null;
    const ok =
      target !== null
        ? await gatewayStore.saveGroup(target, { name: result.name })
        : await gatewayStore.addGroup({ name: result.name });
    groupBusy = false;
    if (!ok) {
      groupError = gatewayStore.error;
      return;
    }
    groupDialogOpen = false;
    editGroup = null;
  }

  // Member ops gate on their own busy flag and surface the refusal
  // in the dialog: the page-level alert sits behind the modal
  // backdrop, so a failed add or remove would otherwise vanish.
  async function addMember(account: string): Promise<void> {
    const groupId = editGroup?.group_id;
    if (!groupId || memberBusy) return;
    memberBusy = true;
    groupError = null;
    await gatewayStore.addMember(groupId, account);
    memberBusy = false;
    if (gatewayStore.error !== null) {
      groupError = gatewayStore.error;
    }
  }

  async function removeMember(account: string): Promise<void> {
    const groupId = editGroup?.group_id;
    if (!groupId || memberBusy) return;
    memberBusy = true;
    groupError = null;
    await gatewayStore.removeMember(groupId, account);
    memberBusy = false;
    if (gatewayStore.error !== null) {
      groupError = gatewayStore.error;
    }
  }

  async function confirmRemoveGroup(): Promise<void> {
    const groupId = removeGroupTarget?.group_id;
    if (!groupId) return;
    removeGroupBusy = true;
    removeGroupError = null;
    await gatewayStore.removeGroup(groupId);
    removeGroupBusy = false;
    if (gatewayStore.error !== null) {
      removeGroupError = gatewayStore.error;
    } else {
      removeGroupTarget = null;
    }
  }

  function openCredentialDialog(row: ProviderRow): void {
    credentialTarget = {
      provider_id: row.provider_id,
      base_url: row.base_url,
    };
    credentialError = null;
  }

  async function saveCredential(result: {
    baseUrl: string;
    apiKey: string;
  }): Promise<void> {
    if (!credentialTarget) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await gatewayStore.saveCredential(
      credentialTarget.provider_id,
      result.baseUrl,
      result.apiKey,
    );
    credentialBusy = false;
    if (!ok) {
      credentialError = gatewayStore.error;
      return;
    }
    credentialTarget = null;
  }

  function requestCredentialRemove(): void {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialTarget = null;
    credentialRemoveError = null;
    credentialRemoveTarget = id;
  }

  async function confirmCredentialRemove(): Promise<void> {
    const id = credentialRemoveTarget;
    if (!id) return;
    credentialRemoveBusy = true;
    credentialRemoveError = null;
    const ok = await gatewayStore.removeCredential(id);
    credentialRemoveBusy = false;
    if (!ok) {
      credentialRemoveError = gatewayStore.error;
    } else {
      credentialRemoveTarget = null;
    }
  }

  async function saveNewSet(result: {
    name: string;
    description: string;
  }): Promise<void> {
    setCreateBusy = true;
    setCreateError = null;
    const ok = await gatewayStore.addSet(
      detailRow?.name ?? "",
      result.name,
      result.description,
    );
    setCreateBusy = false;
    if (!ok) {
      // Keep the dialog open and surface the reason (a duplicate name
      // that slips past the front check is refused with a 409).
      setCreateError = gatewayStore.error;
      return;
    }
    setCreateOpen = false;
  }

  function clearDrag(): void {
    dragProfileId = null;
    dragSourceCollection = null;
    dragOverSet = null;
  }

  async function onDropToSet(setName: string): Promise<void> {
    const profileId = dragProfileId;
    clearDrag();
    if (profileId === null || dropBusy) return;
    dropBusy = true;
    try {
      await gatewayStore.addUnassignedToSet(
        detailRow?.name ?? "",
        profileId,
        setName,
      );
      // Re-read only on success: the reload's own read nulls the error
      // first (store.run), so an unconditional reload would wipe the
      // failure banner before it paints.
      if (gatewayStore.error === null) await reloadDetailUnassigned();
    } finally {
      dropBusy = false;
    }
  }
  // The list view's drop fallback: with no detail open there is no
  // set card to land on, so the drop opens the placement dialog
  // (the dragged profile rides along; the busy latch is shared).
  let placementOpen = $state(false);
  let placementProfileId = $state<string | null>(null);
  let placementSourceCollection = $state<string | null>(null);
  function onDropAnywhere(e: DragEvent): void {
    e.preventDefault();
    const profileId = dragProfileId;
    const sourceCollection = dragSourceCollection;
    clearDrag();
    if (profileId === null) return;
    placementProfileId = profileId;
    placementSourceCollection = sourceCollection;
    placementOpen = true;
  }
  async function confirmPlacement(
    collectionName: string,
    setName: string,
  ): Promise<void> {
    const profileId = placementProfileId;
    if (profileId === null || dropBusy) return;
    dropBusy = true;
    try {
      await gatewayStore.addUnassignedToSet(collectionName, profileId, setName);
      if (gatewayStore.error === null) {
        placementOpen = false;
        placementSourceCollection = null;
      }
    } finally {
      dropBusy = false;
    }
  }
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
              onSelect={(value) => {
                if (value.startsWith("publish:")) {
                  void gatewayStore.publishToGroup(
                    detailRow.name,
                    value.slice("publish:".length),
                  );
                } else if (value.startsWith("unpublish:")) {
                  void gatewayStore.unpublishFromGroup(
                    detailRow.name,
                    value.slice("unpublish:".length),
                  );
                } else if (value === "edit") {
                  openCollectionDialog(detailRow);
                } else if (value === "remove") {
                  removeCollectionTarget = detailRow;
                  removeCollectionError = null;
                }
              }}
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
          onEdit={(set) => void onEditSet(set)}
          onRemove={(set) => {
            removeTarget = set;
            removeError = null;
          }}
          onAdd={() => {
            setCreateOpen = true;
            setCreateError = null;
          }}
          onSetDefault={(set) =>
            void gatewayStore.setCollectionDefault(detailRow.name, set.name)}
          {dragOverSet}
          onSetDragOver={(name) => (dragOverSet = name)}
          onSetDragLeave={(name) =>
            (dragOverSet = name === dragOverSet ? null : dragOverSet)}
          onSetDrop={(name) => void onDropToSet(name)}
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
          ondrop={(e) => onDropAnywhere(e)}
        >
          <CollectionsSection
            collections={gatewayStore.collections ?? []}
            groups={gatewayStore.groups ?? []}
            onAdd={() => openCollectionDialog(null)}
          />
        </div>
      {/if}

      <GroupsSection
        groups={gatewayStore.groups ?? []}
        onEdit={(row) => openGroupDialog(row)}
        onRemove={(row) => {
          removeGroupTarget = row;
          removeGroupError = null;
        }}
        onAdd={() => openGroupDialog(null)}
      />
      <UnassignedSection
        unassigned={detailRow
          ? (detailUnassigned ?? [])
          : (gatewayStore.unassigned ?? [])}
        scope={detailRow?.name ?? null}
        onCreate={() => openUnassignedDialog(null)}
        onEdit={(row) => openUnassignedDialog(row)}
        onDelete={(row) => {
          removeUnassignedTarget = row;
          unassignedError = null;
        }}
        onDragStart={(profile) => {
          dragProfileId = profile.profile_id;
          dragSourceCollection = profile.collection_name;
        }}
        onDragEnd={clearDrag}
      />

      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_gateway_providers()}
        </h2>
        <ProvidersSection
          providers={gatewayStore.providers ?? []}
          onEdit={(row) => openProviderDialog(row)}
          onRemove={(row) => {
            removeProviderTarget = row.provider_id;
            providerError = null;
          }}
          onCreate={() => openProviderDialog(null)}
          onCredential={(row) => openCredentialDialog(row)}
        />
      </section>
    {/if}
  </div>
</div>

<ConfirmDialog
  open={removeTarget !== null}
  title={removeTarget?.name ?? ""}
  description={manage_gateway_remove_set_body({
    name: removeTarget?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={removeBusy}
  error={removeError}
  onConfirm={() => void confirmRemove()}
  onCancel={() => {
    removeTarget = null;
    removeError = null;
  }}
/>

<SetDialog
  open={dialogOpen}
  set={editSet}
  onSave={(result) => void onSaveSet(result)}
  onCancel={() => {
    dialogOpen = false;
    editSet = null;
  }}
/>

<SetCreateDialog
  open={setCreateOpen}
  names={gatewayStore.setsIn(detailRow?.name ?? "").map((s) => s.name)}
  busy={setCreateBusy}
  error={setCreateError}
  onSave={(result) => void saveNewSet(result)}
  onCancel={() => {
    setCreateOpen = false;
    setCreateError = null;
  }}
/>

<ConfirmDialog
  open={removeProviderTarget !== null}
  title={removeProviderTarget ?? ""}
  description={manage_gateway_remove_provider_body({
    name: removeProviderTarget ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={providerBusy}
  error={providerError}
  onConfirm={() => void confirmRemoveProvider()}
  onCancel={() => {
    removeProviderTarget = null;
    providerError = null;
  }}
/>

<ProviderDialog
  open={providerDialogOpen}
  row={editProvider}
  busy={providerBusy}
  error={providerError}
  onSave={(result) => void saveProvider(result)}
  onCancel={() => {
    providerDialogOpen = false;
    editProvider = null;
    providerError = null;
  }}
/>

<ProfileDialog
  open={unassignedDialogOpen}
  providers={gatewayStore.providers ?? []}
  collections={gatewayStore.collections ?? []}
  pinnedCollection={detailRow?.name ?? null}
  row={editUnassigned}
  busy={unassignedBusy}
  error={unassignedError}
  onSave={(result) => void saveUnassigned(result)}
  onCancel={() => {
    unassignedDialogOpen = false;
    editUnassigned = null;
    unassignedError = null;
  }}
/>

<ConfirmDialog
  open={removeUnassignedTarget !== null}
  title={removeUnassignedTarget?.profile_id ?? ""}
  description={manage_gateway_remove_unassigned_body({
    name: removeUnassignedTarget?.profile_id ?? "",
    collection: removeUnassignedTarget?.collection_name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={unassignedBusy}
  error={unassignedError}
  onConfirm={() => void confirmRemoveUnassigned()}
  onCancel={() => {
    removeUnassignedTarget = null;
    unassignedError = null;
  }}
/>

<ProviderCredentialDialog
  open={credentialTarget !== null}
  row={credentialTarget}
  busy={credentialBusy}
  error={credentialError}
  onSave={(result) => void saveCredential(result)}
  onRemove={requestCredentialRemove}
  onCancel={() => {
    credentialTarget = null;
    credentialError = null;
  }}
/>

<ConfirmDialog
  open={credentialRemoveTarget !== null}
  title={credentialRemoveTarget ?? ""}
  description={user_gateway_remove_credential_body({
    id: credentialRemoveTarget ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={credentialRemoveBusy}
  error={credentialRemoveError}
  onConfirm={() => void confirmCredentialRemove()}
  onCancel={() => {
    credentialRemoveTarget = null;
    credentialRemoveError = null;
  }}
/>

<CollectionDialog
  open={collectionDialogOpen}
  collection={editCollection}
  existingNames={(gatewayStore.collections ?? []).map((c) => c.name)}
  busy={collectionBusy}
  error={collectionError}
  onSave={(result) => void saveCollection(result)}
  onCancel={() => {
    collectionDialogOpen = false;
    editCollection = null;
    collectionError = null;
  }}
/>

<GroupDialog
  open={groupDialogOpen}
  group={editGroupLive}
  busy={groupBusy || memberBusy}
  error={groupError}
  onSave={(result) => void saveGroup(result)}
  onAddMember={(account) => void addMember(account)}
  onRemoveMember={(account) => void removeMember(account)}
  onSearch={(query) => searchAccounts(query)}
  onCancel={() => {
    groupDialogOpen = false;
    editGroup = null;
    groupError = null;
  }}
/>

<ConfirmDialog
  open={removeGroupTarget !== null}
  title={removeGroupTarget?.name ?? ""}
  description={manage_gateway_remove_group_body({
    name: removeGroupTarget?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={removeGroupBusy}
  error={removeGroupError}
  onConfirm={() => void confirmRemoveGroup()}
  onCancel={() => {
    removeGroupTarget = null;
    removeGroupError = null;
  }}
/>

<ConfirmDialog
  open={removeCollectionTarget !== null}
  title={removeCollectionTarget?.name ?? ""}
  description={removeCollectionBody}
  confirmLabel={common_remove()}
  tone="danger"
  busy={removeCollectionBusy}
  error={removeCollectionError}
  onConfirm={() => void confirmRemoveCollection()}
  onCancel={() => {
    removeCollectionTarget = null;
    removeCollectionError = null;
  }}
/>

<PlacementDialog
  open={placementOpen}
  profileId={placementProfileId}
  sourceCollection={placementSourceCollection ?? undefined}
  collections={gatewayStore.collections ?? []}
  sets={gatewayStore.sets ?? []}
  busy={dropBusy}
  error={gatewayStore.error}
  onConfirm={(collectionName, setName) =>
    void confirmPlacement(collectionName, setName)}
  onCancel={() => {
    placementOpen = false;
    placementProfileId = null;
    placementSourceCollection = null;
  }}
/>
