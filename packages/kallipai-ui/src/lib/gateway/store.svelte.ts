// UserGatewayStore: the gateway user face's read/write state as a
// class singleton (the manage store family shape). The client module
// stays the wire authority; the store is a thin wrapper that owns the
// error/loading state and re-reads every slice after a mutation -- with
// one exception: a save that spans several requests (the set dialog) is
// refreshed once by its sequence, so those writes only write.

import {
  bakedGatewayUrl,
  createCollection,
  createGroup,
  createProfile,
  createProvider,
  createCollectionSet,
  deleteCollection,
  deleteGroup,
  deleteProfile,
  deleteProvider,
  deleteProviderCredential,
  deleteCollectionSet,
  getCollection,
  getGroup,
  getCollectionSet,
  listCollections,
  listGroups,
  listPlatformCollections,
  listProfiles,
  listProviders,
  listSets,
  publishCollection,
  putProviderCredential,
  unpublishCollection,
  updateCollection,
  updateGroup,
  updateProfile,
  updateProvider,
  updateCollectionSet,
  type UserCollectionRow,
  type UserGroupRow,
  type UserPlatformCollectionRow,
  type UserProfileRow,
  type UserProviderRow,
  type UserSetRow,
} from "./client.ts";
import type { UserMemberBody } from "./userSetDraft.ts";

class UserGatewayStore {
  /** The baked gateway base URL (one origin for both faces). */
  gatewayUrl = $state(bakedGatewayUrl());
  /** The caller's pool rows; null until the first read lands. */
  providers = $state<UserProviderRow[] | null>(null);
  profiles = $state<UserProfileRow[] | null>(null);
  sets = $state<UserSetRow[] | null>(null);
  collections = $state<UserCollectionRow[] | null>(null);
  groups = $state<UserGroupRow[] | null>(null);
  /** The platform collections the caller's reach carries. */
  platformCatalog = $state<UserPlatformCollectionRow[] | null>(null);
  isLoading = $state(false);
  hasLoaded = $state(false);
  error = $state<string | null>(null);

  // A failure nulls the error first, stores the message, and answers
  // undefined so callers distinguish "failed" from a legal
  // null/false result.
  private async run<T>(fn: () => Promise<T>): Promise<T | undefined> {
    this.error = null;
    try {
      return await fn();
    } catch (e) {
      this.error = String(e instanceof Error ? e.message : e);
      return undefined;
    }
  }

  /** Read the whole user space: lists, catalog. */
  async refreshAll(): Promise<void> {
    this.isLoading = true;
    try {
      const [providers, profiles, sets, collections, groups, platformCatalog] =
        await Promise.all([
          this.run(() => listProviders(this.gatewayUrl)),
          this.run(() => listProfiles(undefined, this.gatewayUrl)),
          this.run(() => listSets(undefined, this.gatewayUrl)),
          this.run(() => listCollections(this.gatewayUrl)),
          this.run(() => listGroups(this.gatewayUrl)),
          this.run(() => listPlatformCollections(this.gatewayUrl)),
        ]);
      if (providers) this.providers = providers;
      if (profiles) this.profiles = profiles;
      if (sets) this.sets = sets;
      if (collections) this.collections = collections;
      if (groups) this.groups = groups;
      if (platformCatalog) this.platformCatalog = platformCatalog;
      this.hasLoaded = true;
    } finally {
      this.isLoading = false;
    }
  }

  private collectionsSeq = 0;

  /** Read just the two collection lists (the tagma profiles page's
   * browse preview): own collections plus the platform catalog. A
   * failure leaves the previous rows and parks the message, so the
   * preview can retry without touching the rest of the store. A
   * response superseded by a newer read writes nothing (the seq
   * token drops it), so a stale retry cannot overwrite fresh rows. */
  async loadCollections(): Promise<void> {
    const seq = ++this.collectionsSeq;
    this.isLoading = true;
    try {
      const [collections, platformCatalog] = await Promise.all([
        this.run(() => listCollections(this.gatewayUrl)),
        this.run(() => listPlatformCollections(this.gatewayUrl)),
      ]);
      if (seq !== this.collectionsSeq) return;
      if (collections) this.collections = collections;
      if (platformCatalog) this.platformCatalog = platformCatalog;
    } finally {
      if (seq === this.collectionsSeq) this.isLoading = false;
    }
  }

  // -- providers ----------------------------------------------------------

  async addProvider(body: {
    provider_id: string;
    family: string;
    base_url?: string | null;
    api_key?: string | null;
  }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createProvider(body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async saveProvider(
    providerId: string,
    body: {
      family?: string;
      base_url?: string | null;
      api_key?: string | null;
    },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateProvider(providerId, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async removeProvider(providerId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteProvider(providerId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }
  /** Write or rewrite the provider's credential, then re-read. */
  async saveCredential(
    providerId: string,
    upstreamBaseUrl: string,
    upstreamApiKey: string,
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await putProviderCredential(
        providerId,
        upstreamBaseUrl,
        upstreamApiKey,
        this.gatewayUrl,
      );
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  /** Drop the provider's credential, then re-read. */
  async removeCredential(providerId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteProviderCredential(providerId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  // -- profiles -----------------------------------------------------------

  async addProfile(
    collection: string,
    body: {
      profile_id: string;
      provider_id: string;
      model: string;
      max_context_window?: number | null;
      effort?: string | null;
      modalities?: string[] | null;
      max_budget?: number | null;
      tpm_limit?: number | null;
      rpm_limit?: number | null;
    },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await createProfile(collection, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async saveProfile(
    collection: string,
    profileId: string,
    body: {
      model: string;
      max_context_window?: number | null;
      effort?: string | null;
      modalities?: string[] | null;
      max_budget?: number | null;
      tpm_limit?: number | null;
      rpm_limit?: number | null;
    },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateProfile(collection, profileId, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  /** PUT one member row in place (a set dialog's member edit). The body
   * is the shared fields alone: a user's own rows cannot carry the
   * catalog's served flag. The caller re-reads what it needs. */
  async updateMember(
    collection: string,
    profileId: string,
    body: UserMemberBody,
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateProfile(collection, profileId, body, this.gatewayUrl);
      return true;
    });
    return ok === true;
  }

  async removeProfile(collection: string, profileId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteProfile(collection, profileId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  // -- sets ---------------------------------------------------------------

  /** The full row of a set under the collection that anchors it (the
   * edit dialog's seed). */
  loadSet(collection: string, name: string): Promise<UserSetRow | undefined> {
    return this.run(() => getCollectionSet(collection, name, this.gatewayUrl));
  }

  async addSet(
    collection: string,
    body: {
      name: string;
      description: string;
      members?: string[];
    },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await createCollectionSet(collection, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  /** PATCH description + member order in one request. The write alone: a
   * set save spans several requests, so the read side refreshes once at the
   * end of the sequence (the dialog's settle port), never per write. */
  async saveSet(
    collection: string,
    name: string,
    body: { description?: string; members?: string[] },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateCollectionSet(collection, name, body, this.gatewayUrl);
      return true;
    });
    return ok === true;
  }

  async removeSet(collection: string, name: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteCollectionSet(collection, name, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  // -- collections --------------------------------------------------------

  /** The full row of a collection (the edit dialog's seed). */
  loadCollection(name: string): Promise<UserCollectionRow | undefined> {
    return this.run(() => getCollection(name, this.gatewayUrl));
  }

  async addCollection(body: {
    name: string;
    description: string;
    sets?: string[];
  }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createCollection(body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async saveCollection(
    name: string,
    body: { description?: string; sets?: string[] },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateCollection(name, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async removeCollection(name: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteCollection(name, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async publish(name: string, groupId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await publishCollection(name, groupId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async unpublish(name: string, groupId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await unpublishCollection(name, groupId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  // -- groups -------------------------------------------------------------

  /** The full row of a group (the edit dialog's seed). */
  loadGroup(groupId: string): Promise<UserGroupRow | undefined> {
    return this.run(() => getGroup(groupId, this.gatewayUrl));
  }

  async addGroup(body: { name: string; members?: string[] }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createGroup(body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async saveGroup(
    groupId: string,
    body: { name?: string; members?: string[] },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateGroup(groupId, body, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async removeGroup(groupId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteGroup(groupId, this.gatewayUrl);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }
}

export const userGatewayStore = new UserGatewayStore();
