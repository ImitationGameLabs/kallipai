<script lang="ts">
  // The caller's model gateway space (/gateway): the user face over
  // the shared registry. Every signed-in account qualifies -- an
  // admin session manages its own user domain here. Every section
  // edits with immediate effect: ConfirmDialog on removes,
  // dialog-scoped drafts for the composite edits, and the reserved
  // audience rendered as the publish dialog's built-in option.
  //
  // The page mounts the sections, owns the one-shot read, and hands each
  // dialog group its controller: the controllers hold the targets, the
  // latches and the request tokens, so nothing here assigns dialog state.
  // A cancel arrives as the controller's method, not as a closure: the
  // dialogs hand their close event over, and a cancel takes no argument.
  import { userGatewayStore } from "../../lib/gateway/store.svelte.ts";
  import { createMineCollectionDialogs } from "../../lib/gateway/mineCollectionDialogs.svelte.ts";
  import { createMineGroupDialogs } from "../../lib/gateway/mineGroupDialogs.svelte.ts";
  import { createMineProfileDialogs } from "../../lib/gateway/mineProfileDialogs.svelte.ts";
  import { createMineProviderDialogs } from "../../lib/gateway/mineProviderDialogs.svelte.ts";
  import { createMineSetDialogs } from "../../lib/gateway/mineSetDialogs.svelte.ts";
  import PageHeader from "../../components/PageHeader.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import UserProvidersSection from "../../components/gateway/UserProvidersSection.svelte";
  import UserProviderDialog from "../../components/gateway/UserProviderDialog.svelte";
  import UserCredentialDialog from "../../components/gateway/UserCredentialDialog.svelte";
  import UserProfilesSection from "../../components/gateway/UserProfilesSection.svelte";
  import UserProfileDialog from "../../components/gateway/UserProfileDialog.svelte";
  import UserSetsSection from "../../components/gateway/UserSetsSection.svelte";
  import UserSetDialog from "../../components/gateway/UserSetDialog.svelte";
  import UserCollectionsSection from "../../components/gateway/UserCollectionsSection.svelte";
  import UserCollectionDialog from "../../components/gateway/UserCollectionDialog.svelte";
  import UserPublishDialog from "../../components/gateway/UserPublishDialog.svelte";
  import UserGroupsSection from "../../components/gateway/UserGroupsSection.svelte";
  import UserGroupDialog from "../../components/gateway/UserGroupDialog.svelte";
  import UserPlatformCatalogSection from "../../components/gateway/UserPlatformCatalogSection.svelte";
  import {
    common_remove,
    user_gateway_heading,
    user_gateway_remove_credential_body,
    user_gateway_remove_group_body,
    user_gateway_remove_profile_body,
    user_gateway_remove_provider_body,
    user_gateway_remove_set_body,
    user_gateway_subtitle,
    user_gateway_title,
  } from "../../paraglide/messages.js";

  // One-shot read-side load when the page mounts (the store's isLoading
  // dedupes concurrent calls).
  let initialLoadDone = $state(false);
  $effect(() => {
    if (!initialLoadDone) {
      initialLoadDone = true;
      void userGatewayStore.refreshAll();
    }
  });

  // Each group owns its dialogs' targets and latches; the page hands over
  // the store and the rows a set edit projects onto.
  const providerDialogs = createMineProviderDialogs({
    store: userGatewayStore,
  });
  const profileDialogs = createMineProfileDialogs({ store: userGatewayStore });
  const setDialogs = createMineSetDialogs({ store: userGatewayStore });
  const collectionDialogs = createMineCollectionDialogs({
    store: userGatewayStore,
  });
  const groupDialogs = createMineGroupDialogs({ store: userGatewayStore });
</script>

<svelte:head><title>{user_gateway_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <PageHeader>
    {#snippet title()}
      <h1 class="text-sm font-semibold truncate">
        {user_gateway_heading()}
      </h1>
      <p class="text-xs opacity-50 truncate">{user_gateway_subtitle()}</p>
    {/snippet}
  </PageHeader>

  <div class="px-2 md:p-6 max-w-3xl space-y-6">
    {#if userGatewayStore.error}
      <p class="text-sm text-error-500" role="alert">
        {userGatewayStore.error}
      </p>
    {/if}

    <UserProvidersSection
      providers={userGatewayStore.providers ?? []}
      onEdit={(row) => providerDialogs.provider.show(row)}
      onCredential={(row) => providerDialogs.credential.show(row)}
      onRemove={(row) => providerDialogs.provider.remove.ask(row.provider_id)}
      onCreate={() => providerDialogs.provider.show(null)}
    />

    <UserProfilesSection
      profiles={userGatewayStore.profiles ?? []}
      onEdit={(row) => profileDialogs.show(row)}
      onRemove={(row) => profileDialogs.remove.ask(row)}
      onCreate={() => profileDialogs.show(null)}
    />

    <UserSetsSection
      sets={userGatewayStore.sets ?? []}
      collections={userGatewayStore.collections ?? []}
      onEdit={(set) => void setDialogs.edit(set)}
      onRemove={(set) => setDialogs.remove.ask(set)}
      onCreate={(collection) => setDialogs.create(collection)}
    />

    <UserCollectionsSection
      collections={userGatewayStore.collections ?? []}
      onEdit={(row) => void collectionDialogs.edit.show(row)}
      onRemove={(row) => collectionDialogs.remove.ask(row)}
      onPublish={(row) => collectionDialogs.publish.show(row, "publish")}
      onCreate={() => void collectionDialogs.edit.show(null)}
    />

    <UserGroupsSection
      groups={userGatewayStore.groups ?? []}
      onEdit={(row) => void groupDialogs.edit.show(row)}
      onRemove={(row) => groupDialogs.remove.ask(row)}
      onCreate={() => void groupDialogs.edit.show(null)}
    />

    {#if userGatewayStore.platformCatalog !== null && userGatewayStore.platformCatalog.length > 0}
      <UserPlatformCatalogSection
        collections={userGatewayStore.platformCatalog}
      />
    {/if}
  </div>
</div>

<UserProviderDialog
  open={providerDialogs.provider.open}
  row={providerDialogs.provider.row}
  busy={providerDialogs.provider.busy}
  error={providerDialogs.provider.error}
  onSave={(result) => void providerDialogs.provider.save(result)}
  onCancel={providerDialogs.provider.cancel}
/>

<UserCredentialDialog
  open={providerDialogs.credential.open}
  row={providerDialogs.credential.row}
  busy={providerDialogs.credential.busy}
  error={providerDialogs.credential.error}
  onSave={(result) => void providerDialogs.credential.save(result)}
  onRemove={() => providerDialogs.credential.remove.ask()}
  onCancel={providerDialogs.credential.cancel}
/>

<UserProfileDialog
  open={profileDialogs.open}
  providers={userGatewayStore.providers ?? []}
  row={profileDialogs.row}
  busy={profileDialogs.busy}
  error={profileDialogs.error}
  onSave={(result) => void profileDialogs.save(result)}
  onCancel={profileDialogs.cancel}
/>

<UserSetDialog
  open={setDialogs.open}
  set={setDialogs.set}
  collection={setDialogs.collection}
  profiles={setDialogs.profiles}
  busy={setDialogs.busy}
  error={setDialogs.error}
  landed={setDialogs.landed}
  onSave={(result) => void setDialogs.save(result)}
  onCancel={setDialogs.cancel}
/>

<UserCollectionDialog
  open={collectionDialogs.edit.open}
  collection={collectionDialogs.edit.row}
  onSave={collectionDialogs.edit.save}
  onCancel={collectionDialogs.edit.cancel}
/>

<UserPublishDialog
  open={collectionDialogs.publish.open}
  collection={collectionDialogs.publish.target}
  groups={userGatewayStore.groups ?? []}
  mode={collectionDialogs.publish.mode}
  busy={collectionDialogs.publish.busy}
  error={collectionDialogs.publish.error}
  onConfirm={(groupId) => void collectionDialogs.publish.confirm(groupId)}
  onCancel={collectionDialogs.publish.cancel}
/>

<UserGroupDialog
  open={groupDialogs.edit.open}
  row={groupDialogs.edit.row}
  busy={groupDialogs.edit.busy}
  error={groupDialogs.edit.error}
  onSave={(result) => void groupDialogs.edit.save(result)}
  onCancel={groupDialogs.edit.cancel}
/>

<ConfirmDialog
  open={providerDialogs.provider.remove.open}
  title={providerDialogs.provider.remove.target ?? ""}
  description={user_gateway_remove_provider_body({
    name: providerDialogs.provider.remove.target ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={providerDialogs.provider.remove.busy}
  error={providerDialogs.provider.remove.error}
  onConfirm={() => void providerDialogs.provider.remove.confirm()}
  onCancel={providerDialogs.provider.remove.cancel}
/>

<ConfirmDialog
  open={providerDialogs.credential.remove.open}
  title={providerDialogs.credential.row?.provider_id ?? ""}
  description={user_gateway_remove_credential_body({
    id: providerDialogs.credential.row?.provider_id ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={providerDialogs.credential.remove.busy}
  error={providerDialogs.credential.remove.error}
  onConfirm={() => void providerDialogs.credential.remove.confirm()}
  onCancel={providerDialogs.credential.remove.cancel}
/>

<ConfirmDialog
  open={profileDialogs.remove.open}
  title={profileDialogs.remove.target?.profile_id ?? ""}
  description={user_gateway_remove_profile_body({
    name: profileDialogs.remove.target?.profile_id ?? "",
    collection: profileDialogs.remove.target?.collection_name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={profileDialogs.remove.busy}
  error={profileDialogs.remove.error}
  onConfirm={() => void profileDialogs.remove.confirm()}
  onCancel={profileDialogs.remove.cancel}
/>

<ConfirmDialog
  open={setDialogs.remove.open}
  title={setDialogs.remove.target?.name ?? ""}
  description={user_gateway_remove_set_body({
    name: setDialogs.remove.target?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={setDialogs.remove.busy}
  error={setDialogs.remove.error}
  onConfirm={() => void setDialogs.remove.confirm()}
  onCancel={setDialogs.remove.cancel}
/>

<ConfirmDialog
  open={collectionDialogs.remove.open}
  title={collectionDialogs.remove.target?.name ?? ""}
  description={collectionDialogs.remove.body}
  confirmLabel={common_remove()}
  tone="danger"
  busy={collectionDialogs.remove.busy}
  error={collectionDialogs.remove.error}
  onConfirm={() => void collectionDialogs.remove.confirm()}
  onCancel={collectionDialogs.remove.cancel}
/>

<ConfirmDialog
  open={groupDialogs.remove.open}
  title={groupDialogs.remove.target?.name ?? ""}
  description={user_gateway_remove_group_body({
    name: groupDialogs.remove.target?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={groupDialogs.remove.busy}
  error={groupDialogs.remove.error}
  onConfirm={() => void groupDialogs.remove.confirm()}
  onCancel={groupDialogs.remove.cancel}
/>
