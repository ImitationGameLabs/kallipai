<script lang="ts">
  // The caller's model gateway space (/gateway): the user face over
  // the shared registry. Every signed-in account qualifies -- an
  // admin session manages its own user domain here. Every section
  // edits with immediate effect: ConfirmDialog on removes,
  // dialog-scoped drafts for the composite edits, and the reserved
  // audience rendered as the publish dialog's built-in option.
  import { userGatewayStore } from "../../lib/gateway/store.svelte.ts";
  import type {
    UserCollectionRow,
    UserProviderRow,
    UserGroupRow,
    UserProfileRow,
    UserSetRow,
  } from "../../lib/gateway/client.ts";
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
    user_gateway_remove_collection_body,
    user_gateway_remove_credential_body,
    user_gateway_remove_group_body,
    user_gateway_remove_profile_body,
    user_gateway_remove_provider_body,
    user_gateway_remove_set_body,
    user_gateway_subtitle,
    user_gateway_title,
  } from "../../paraglide/messages.js";

  import {
    saveSetDraft,
    saveStoppedBanner,
  } from "../../lib/manage/gateway/saveSet.ts";
  import {
    membershipWrites,
    rowBodyOf,
    type DraftRow,
    type SetSaveDraft,
  } from "../../lib/manage/gateway/setDraft.ts";
  import {
    pickedRows,
    setDetailFor,
    type UserSetDetail,
  } from "../../lib/gateway/userSetDraft.ts";

  // One-shot read-side load when the page mounts (the store's
  // store's isLoading dedupes concurrent calls).
  let initialLoadDone = $state(false);
  $effect(() => {
    if (!initialLoadDone) {
      initialLoadDone = true;
      void userGatewayStore.refreshAll();
    }
  });

  // -- providers ---------------------------------------------------------

  // The provider pool's dialog state. editProvider null = the create
  // form; the dialog error surfaces the store's reason (a referenced
  // provider refuses to delete with a 409).
  let editProvider = $state<{
    provider_id: string;
    family: string;
    base_url: string | null;
  } | null>(null);
  let providerDialogOpen = $state(false);
  let providerBusy = $state(false);
  let providerError = $state<string | null>(null);
  let removeProviderTarget = $state<string | null>(null);

  // The credential dialog's target and its local error (the blank-key
  // and relative-URL refusals are client-side; the rest arrive from
  // the server's envelope).
  let credentialTarget = $state<UserProviderRow | null>(null);
  let credentialOpen = $state(false);
  let credentialBusy = $state(false);
  let credentialError = $state<string | null>(null);
  let credentialConfirmOpen = $state(false);

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
      const ok = await userGatewayStore.addProvider(result);
      providerBusy = false;
      if (!ok) {
        providerError = userGatewayStore.error;
        return;
      }
    } else {
      const ok = await userGatewayStore.saveProvider(editProvider.provider_id, {
        family: result.family,
        base_url: result.base_url,
        api_key: result.api_key,
      });
      providerBusy = false;
      if (!ok) {
        providerError = userGatewayStore.error;
        return;
      }
    }
    providerDialogOpen = false;
    editProvider = null;
  }

  function openCredentialDialog(row: UserProviderRow): void {
    credentialTarget = row;
    credentialError = null;
    credentialOpen = true;
  }

  async function saveCredential(result: {
    baseUrl: string;
    apiKey: string;
  }): Promise<void> {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await userGatewayStore.saveCredential(
      id,
      result.baseUrl,
      result.apiKey,
    );
    credentialBusy = false;
    if (!ok) {
      credentialError = userGatewayStore.error;
      return;
    }
    credentialOpen = false;
    credentialTarget = null;
  }

  async function confirmRemoveCredential(): Promise<void> {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await userGatewayStore.removeCredential(id);
    credentialBusy = false;
    if (!ok) {
      credentialError = userGatewayStore.error;
      return;
    }
    credentialOpen = false;
    credentialTarget = null;
    credentialConfirmOpen = false;
  }

  async function confirmRemoveProvider(): Promise<void> {
    const id = removeProviderTarget;
    if (!id) return;
    providerBusy = true;
    providerError = null;
    await userGatewayStore.removeProvider(id);
    providerBusy = false;
    if (userGatewayStore.error !== null) {
      // Keep the dialog open and surface the reason (a referenced
      // provider refuses to delete with a 409).
      providerError = userGatewayStore.error;
    } else {
      removeProviderTarget = null;
    }
  }

  // -- profiles ----------------------------------------------------------

  let editProfile = $state<UserProfileRow | null>(null);
  let profileDialogOpen = $state(false);
  let profileBusy = $state(false);
  let profileError = $state<string | null>(null);
  let removeProfileTarget = $state<UserProfileRow | null>(null);

  function openProfileDialog(row: UserProfileRow | null): void {
    editProfile = row;
    profileError = null;
    profileDialogOpen = true;
  }

  async function saveProfile(result: {
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
    profileBusy = true;
    profileError = null;
    const body = {
      model: result.model,
      max_context_window: result.max_context_window,
      effort: result.effort,
      modalities: result.modalities,
      max_budget: result.max_budget,
      tpm_limit: result.tpm_limit,
      rpm_limit: result.rpm_limit,
    };
    const ok =
      editProfile === null
        ? await userGatewayStore.addProfile(result.collection, {
            profile_id: result.profile_id,
            provider_id: result.provider_id,
            ...body,
          })
        : await userGatewayStore.saveProfile(
            editProfile.collection_name,
            editProfile.profile_id,
            rowBodyOf(editProfile, body),
          );
    profileBusy = false;
    if (!ok) {
      profileError = userGatewayStore.error;
      return;
    }
    profileDialogOpen = false;
    editProfile = null;
  }

  async function confirmRemoveProfile(): Promise<void> {
    const row = removeProfileTarget;
    if (row === null) return;
    profileBusy = true;
    profileError = null;
    await userGatewayStore.removeProfile(row.collection_name, row.profile_id);
    profileBusy = false;
    if (userGatewayStore.error !== null) {
      // Keep the confirm open and surface the reason.
      profileError = userGatewayStore.error;
    } else {
      removeProfileTarget = null;
    }
  }

  // -- sets ---------------------------------------------------------------

  // Each edit click takes a request token; only the newest token's
  // response may seed the dialog (the admin console's stale-response
  // guard).
  let editSet = $state<UserSetDetail | null>(null);
  let setDialogOpen = $state(false);
  let setEditRequest = $state(0);
  let removeSetTarget = $state<UserSetRow | null>(null);
  let removeSetBusy = $state(false);
  let removeSetError = $state<string | null>(null);
  // The create dialog's pre-bound collection (set by the group the
  // create was opened from).
  let createCollection = $state("");
  // The save in flight and what a stopped save reports: the dialog stays
  // open on a failure, so these ride it rather than the page banner.
  let setDialogBusy = $state(false);
  let setDialogError = $state<string | null>(null);
  let setDialogLanded = $state<readonly string[]>([]);
  /** The collection the set dialog is working in: the row under edit, or
   * the one a create was opened from. */
  const activeCollection = $derived(
    editSet === null ? createCollection : editSet.collection_name,
  );

  /** The collection's profile rows: the dialog's member pool and the rows
   * a member id resolves to. Null while the store has not read the rows:
   * rows never read are not rows known to be absent, and a member list
   * projected onto unknown rows would read every member as deleted. */
  function collectionProfiles(
    collection: string,
  ): readonly UserProfileRow[] | null {
    const rows = userGatewayStore.profiles;
    if (rows === null) return null;
    return rows.filter((row) => row.collection_name === collection);
  }

  async function onEditSet(set: UserSetRow): Promise<void> {
    const request = ++setEditRequest;
    const row = await userGatewayStore.loadSet(set.collection_name, set.name);
    if (row === undefined || request !== setEditRequest) return;
    if (userGatewayStore.profiles === null) {
      // The projection needs the rows: with them unread the detail comes
      // out empty and the first save would clear the set. Read once, then
      // open only when the rows are known; the page alert carries the
      // read's reason.
      await userGatewayStore.refreshAll();
      if (request !== setEditRequest) return;
    }
    const detail = setDetailFor(row, collectionProfiles(row.collection_name));
    if (detail === null) return;
    editSet = detail;
    setDialogOpen = true;
  }

  // One save spans several requests, so the read side refreshes once, in
  // the sequence (the settle port below), never per write: the store's
  // updateMember, patchDescription and reorderSet only write.
  async function onSaveSet(result: SetSaveDraft): Promise<void> {
    const editing = editSet;
    setDialogBusy = true;
    setDialogError = null;
    try {
      let target: UserSetDetail | null = editing;
      let draft = result;
      if (target === null) {
        // A create registers an empty set, then walks the same sequence:
        // the member PUTs are what put rows in it. An empty set is a legal
        // create, so the picks' rows are resolved only when there are
        // picks: a pick names a row of the collection the set lands in,
        // and rows never read are not rows known to be absent. Read once,
        // then refuse before the write; the page alert carries the read's
        // reason.
        let picked: DraftRow[] = [];
        if (result.members.length > 0) {
          let source = collectionProfiles(createCollection);
          if (source === null) {
            await userGatewayStore.refreshAll();
            source = collectionProfiles(createCollection);
          }
          if (source === null) {
            setDialogError = userGatewayStore.error;
            return;
          }
          picked = pickedRows(result.members, source);
        }
        const created = await userGatewayStore.addSet(createCollection, {
          name: result.name,
          description: result.description,
        });
        if (created === undefined) {
          setDialogError = userGatewayStore.error;
          return;
        }
        target = {
          collection_name: created.collection_name,
          name: created.name,
          description: created.description,
          profiles: [],
        };
        draft = {
          ...result,
          name: created.name,
          ...membershipWrites([], picked, {}, created.name),
        };
      }
      const anchor = target;
      const outcome = await saveSetDraft(anchor, draft, {
        putMember: async (profileId, body) => {
          const ok = await userGatewayStore.updateMember(
            anchor.collection_name,
            profileId,
            body,
          );
          return { ok, reason: ok ? null : userGatewayStore.error };
        },
        patchDescription: async (description) => {
          const ok = await userGatewayStore.patchDescription(
            anchor.collection_name,
            anchor.name,
            description,
          );
          return { ok, reason: ok ? null : userGatewayStore.error };
        },
        reorder: async (members) => {
          const ok = await userGatewayStore.reorderSet(
            anchor.collection_name,
            anchor.name,
            members,
          );
          return { ok, reason: ok ? null : userGatewayStore.error };
        },
        // The rows a PUT just changed are not in the page's cache yet: the
        // re-read below projects them, so the refresh has to land first.
        settle: () => userGatewayStore.refreshAll(),
        reread: async () => {
          const row = await userGatewayStore.loadSet(
            anchor.collection_name,
            anchor.name,
          );
          if (row === undefined) return undefined;
          // Rows still unread answer undefined: the draft keeps what it has
          // rather than reseeding from an empty member list.
          return (
            setDetailFor(row, collectionProfiles(anchor.collection_name)) ??
            undefined
          );
        },
      });
      if (outcome.phase !== "done") {
        // The landed list lands before the re-read row: the dialog's
        // effect short-circuits on set === seeded, so these two
        // assignments are not interchangeable.
        setDialogLanded = outcome.landed;
        setDialogError = saveStoppedBanner(outcome);
        if (outcome.row) editSet = outcome.row;
        return;
      }
      setDialogOpen = false;
      editSet = null;
      setDialogLanded = [];
    } finally {
      setDialogBusy = false;
    }
  }

  async function confirmRemoveSet(): Promise<void> {
    const row = removeSetTarget;
    if (row === null) return;
    removeSetBusy = true;
    removeSetError = null;
    await userGatewayStore.removeSet(row.collection_name, row.name);
    removeSetBusy = false;
    if (userGatewayStore.error !== null) {
      removeSetError = userGatewayStore.error;
    } else {
      removeSetTarget = null;
    }
  }

  // -- collections --------------------------------------------------------

  let editCollection = $state<UserCollectionRow | null>(null);
  let collectionDialogOpen = $state(false);
  let collectionEditRequest = $state(0);
  let removeCollectionTarget = $state<UserCollectionRow | null>(null);
  // The delete confirm's copy, built from the row: both counts are the
  // server's numbers, so the dialog quotes what the cascade will take.
  const removeCollectionBody = $derived(
    removeCollectionTarget === null
      ? ""
      : user_gateway_remove_collection_body({
          name: removeCollectionTarget.name,
          sets: removeCollectionTarget.set_count,
          profiles: removeCollectionTarget.profile_count,
        }),
  );
  let removeCollectionBusy = $state(false);
  let removeCollectionError = $state<string | null>(null);

  // The publish dialog: mode picks publish or unpublish; both answer
  // through the store and surface the server's reason (an already
  // published collection conflicts with a 409).
  let publishTarget = $state<UserCollectionRow | null>(null);
  let publishMode = $state<"publish" | "unpublish">("publish");
  let publishOpen = $state(false);
  let publishBusy = $state(false);
  let publishError = $state<string | null>(null);

  async function onEditCollection(row: UserCollectionRow): Promise<void> {
    const request = ++collectionEditRequest;
    const full = await userGatewayStore.loadCollection(row.name);
    if (full && request === collectionEditRequest) {
      editCollection = full;
      collectionDialogOpen = true;
    }
  }

  function onSaveCollection(result: {
    description: string;
    sets: string[];
  }): void {
    const name = editCollection?.name;
    collectionDialogOpen = false;
    editCollection = null;
    if (name) {
      void userGatewayStore.saveCollection(name, {
        description: result.description,
        sets: result.sets,
      });
    }
  }

  function openPublishDialog(
    row: UserCollectionRow,
    mode: "publish" | "unpublish",
  ): void {
    publishTarget = row;
    publishMode = mode;
    publishError = null;
    publishOpen = true;
  }

  async function confirmPublish(groupId: string): Promise<void> {
    const name = publishTarget?.name;
    if (!name) return;
    publishBusy = true;
    publishError = null;
    const ok =
      publishMode === "publish"
        ? await userGatewayStore.publish(name, groupId)
        : await userGatewayStore.unpublish(name, groupId);
    publishBusy = false;
    if (!ok) {
      publishError = userGatewayStore.error;
      return;
    }
    publishOpen = false;
    publishTarget = null;
  }

  async function confirmRemoveCollection(): Promise<void> {
    const name = removeCollectionTarget?.name;
    if (!name) return;
    removeCollectionBusy = true;
    removeCollectionError = null;
    await userGatewayStore.removeCollection(name);
    removeCollectionBusy = false;
    if (userGatewayStore.error !== null) {
      removeCollectionError = userGatewayStore.error;
    } else {
      removeCollectionTarget = null;
    }
  }

  // -- groups ---------------------------------------------------------------

  let editGroup = $state<UserGroupRow | null>(null);
  let groupDialogOpen = $state(false);
  let groupBusy = $state(false);
  let groupError = $state<string | null>(null);
  let removeGroupTarget = $state<UserGroupRow | null>(null);

  async function onEditGroup(row: UserGroupRow): Promise<void> {
    groupBusy = false;
    groupError = null;
    const full = await userGatewayStore.loadGroup(row.group_id);
    if (full) {
      editGroup = full;
      groupDialogOpen = true;
    }
  }

  async function saveGroup(result: {
    name: string;
    members: string[];
  }): Promise<void> {
    groupBusy = true;
    groupError = null;
    const ok =
      editGroup === null
        ? await userGatewayStore.addGroup({
            name: result.name,
            members: result.members,
          })
        : await userGatewayStore.saveGroup(editGroup.group_id, {
            name: result.name,
            members: result.members,
          });
    groupBusy = false;
    if (!ok) {
      groupError = userGatewayStore.error;
      return;
    }
    groupDialogOpen = false;
    editGroup = null;
  }

  async function confirmRemoveGroup(): Promise<void> {
    const id = removeGroupTarget?.group_id;
    if (!id) return;
    groupBusy = true;
    groupError = null;
    await userGatewayStore.removeGroup(id);
    groupBusy = false;
    if (userGatewayStore.error !== null) {
      groupError = userGatewayStore.error;
    } else {
      removeGroupTarget = null;
    }
  }
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
      onEdit={(row) => openProviderDialog(row)}
      onCredential={(row) => openCredentialDialog(row)}
      onRemove={(row) => {
        removeProviderTarget = row.provider_id;
        providerError = null;
      }}
      onCreate={() => openProviderDialog(null)}
    />

    <UserProfilesSection
      profiles={userGatewayStore.profiles ?? []}
      onEdit={(row) => openProfileDialog(row)}
      onRemove={(row) => {
        removeProfileTarget = row;
        profileError = null;
      }}
      onCreate={() => openProfileDialog(null)}
    />

    <UserSetsSection
      sets={userGatewayStore.sets ?? []}
      collections={userGatewayStore.collections ?? []}
      onEdit={(set) => void onEditSet(set)}
      onRemove={(set) => {
        removeSetTarget = set;
        removeSetError = null;
      }}
      onCreate={(collection) => {
        editSet = null;
        setDialogOpen = true;
        createCollection = collection;
      }}
    />

    <UserCollectionsSection
      collections={userGatewayStore.collections ?? []}
      onEdit={(row) => void onEditCollection(row)}
      onRemove={(row) => {
        removeCollectionTarget = row;
        removeCollectionError = null;
      }}
      onPublish={(row) => openPublishDialog(row, "publish")}
      onCreate={() => {
        editCollection = null;
        collectionDialogOpen = true;
      }}
    />

    <UserGroupsSection
      groups={userGatewayStore.groups ?? []}
      onEdit={(row) => void onEditGroup(row)}
      onRemove={(row) => {
        removeGroupTarget = row;
        groupError = null;
      }}
      onCreate={() => {
        editGroup = null;
        groupDialogOpen = true;
      }}
    />

    {#if userGatewayStore.platformCatalog !== null && userGatewayStore.platformCatalog.length > 0}
      <UserPlatformCatalogSection
        collections={userGatewayStore.platformCatalog}
      />
    {/if}
  </div>
</div>

<UserProviderDialog
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

<UserCredentialDialog
  open={credentialOpen}
  row={credentialTarget}
  busy={credentialBusy}
  error={credentialError}
  onSave={(result) => void saveCredential(result)}
  onRemove={() => (credentialConfirmOpen = true)}
  onCancel={() => {
    credentialOpen = false;
    credentialTarget = null;
    credentialError = null;
  }}
/>

<UserProfileDialog
  open={profileDialogOpen}
  providers={userGatewayStore.providers ?? []}
  row={editProfile}
  busy={profileBusy}
  error={profileError}
  onSave={(result) => void saveProfile(result)}
  onCancel={() => {
    profileDialogOpen = false;
    editProfile = null;
    profileError = null;
  }}
/>

<UserSetDialog
  open={setDialogOpen}
  set={editSet}
  collection={activeCollection}
  profiles={collectionProfiles(activeCollection)}
  busy={setDialogBusy}
  error={setDialogError}
  landed={setDialogLanded}
  onSave={(result) => void onSaveSet(result)}
  onCancel={() => {
    if (setDialogBusy) return;
    setDialogOpen = false;
    editSet = null;
    setDialogError = null;
  }}
/>

<UserCollectionDialog
  open={collectionDialogOpen}
  collection={editCollection}
  onSave={onSaveCollection}
  onCancel={() => {
    collectionDialogOpen = false;
    editCollection = null;
  }}
/>

<UserPublishDialog
  open={publishOpen}
  collection={publishTarget}
  groups={userGatewayStore.groups ?? []}
  mode={publishMode}
  busy={publishBusy}
  error={publishError}
  onConfirm={(groupId) => void confirmPublish(groupId)}
  onCancel={() => {
    publishOpen = false;
    publishTarget = null;
    publishError = null;
  }}
/>

<UserGroupDialog
  open={groupDialogOpen}
  row={editGroup}
  busy={groupBusy}
  error={groupError}
  onSave={(result) => void saveGroup(result)}
  onCancel={() => {
    groupDialogOpen = false;
    editGroup = null;
    groupError = null;
  }}
/>

<ConfirmDialog
  open={removeProviderTarget !== null}
  title={removeProviderTarget ?? ""}
  description={user_gateway_remove_provider_body({
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

<ConfirmDialog
  open={credentialConfirmOpen}
  title={credentialTarget?.provider_id ?? ""}
  description={user_gateway_remove_credential_body({
    id: credentialTarget?.provider_id ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={credentialBusy}
  error={credentialError}
  onConfirm={() => void confirmRemoveCredential()}
  onCancel={() => {
    credentialConfirmOpen = false;
    credentialError = null;
  }}
/>

<ConfirmDialog
  open={removeProfileTarget !== null}
  title={removeProfileTarget?.profile_id ?? ""}
  description={user_gateway_remove_profile_body({
    name: removeProfileTarget?.profile_id ?? "",
    collection: removeProfileTarget?.collection_name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={profileBusy}
  error={profileError}
  onConfirm={() => void confirmRemoveProfile()}
  onCancel={() => {
    removeProfileTarget = null;
    profileError = null;
  }}
/>

<ConfirmDialog
  open={removeSetTarget !== null}
  title={removeSetTarget?.name ?? ""}
  description={user_gateway_remove_set_body({
    name: removeSetTarget?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={removeSetBusy}
  error={removeSetError}
  onConfirm={() => void confirmRemoveSet()}
  onCancel={() => {
    removeSetTarget = null;
    removeSetError = null;
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

<ConfirmDialog
  open={removeGroupTarget !== null}
  title={removeGroupTarget?.name ?? ""}
  description={user_gateway_remove_group_body({
    name: removeGroupTarget?.name ?? "",
  })}
  confirmLabel={common_remove()}
  tone="danger"
  busy={groupBusy}
  error={groupError}
  onConfirm={() => void confirmRemoveGroup()}
  onCancel={() => {
    removeGroupTarget = null;
    groupError = null;
  }}
/>
