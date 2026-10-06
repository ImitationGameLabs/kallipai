// GatewayAdminStore: the gateway admin face's read/write state as a
// class singleton (the manage store family shape). The client module
// stays the wire authority; the store is a thin wrapper that owns the
// error/loading state and re-reads the affected slice after each
// mutation -- every action lands on the registry directly.

import {
  addGroupMember,
  type AdminCollectionRow,
  type AdminProfileRow,
  type AdminSetRow,
  createCollection,
  createGroup,
  createParked,
  createProvider,
  createSet,
  deleteCollection,
  deleteGroup,
  deleteParked,
  deleteProvider,
  deleteProviderCredential,
  deleteSet,
  getSet,
  type GroupRow,
  listCollections,
  listGroups,
  listParking,
  listProviders,
  listCollectionSets,
  type ProviderRow,
  publishCollection,
  putProviderCredential,
  removeGroupMember,
  setCollectionDefault,
  unpublishCollection,
  updateCollection,
  updateGroup,
  updateParked,
  updateProvider,
  updateSet,
} from "./client.ts";

class GatewayAdminStore {
  /** The catalog's sets; null until the first read lands. */
  sets = $state<AdminSetRow[] | null>(null);
  /** The provider pool (the catalog space's rows); null until first read. */
  providers = $state<ProviderRow[] | null>(null);
  /** The parked profile rows (the catalog space's drafts). */
  parking = $state<AdminProfileRow[] | null>(null);
  /** The catalog's collections with their live publications. */
  collections = $state<AdminCollectionRow[] | null>(null);
  /** The platform groups (the publish audiences). */
  groups = $state<GroupRow[] | null>(null);
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

  /** Read the whole read side: sets (nested per collection),
   * providers, parking (the active owner filter's slice). */
  async refreshAll(): Promise<void> {
    this.isLoading = true;
    try {
      const [providersBody, parkingBody, collectionsBody, groupsBody] =
        await Promise.all([
          this.run(() => listProviders()),
          this.run(() => listParking()),
          this.run(() => listCollections()),
          this.run(() => listGroups()),
        ]);
      if (collectionsBody) {
        const perCollection = await Promise.all(
          collectionsBody.map((c) =>
            this.run(() => listCollectionSets(c.name)),
          ),
        );
        this.sets = perCollection
          .filter((s): s is AdminSetRow[] => s !== undefined)
          .flat();
      }
      if (providersBody) this.providers = providersBody;
      if (parkingBody) this.parking = parkingBody;
      if (collectionsBody) this.collections = collectionsBody;
      if (groupsBody) this.groups = groupsBody;
      this.hasLoaded = true;
    } finally {
      this.isLoading = false;
    }
  }

  /** Mint a pool row (optionally with its seed credential), then re-read. */
  async addProvider(body: {
    provider_id: string;
    family: string;
    base_url?: string | null;
    api_key?: string | null;
  }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createProvider(body);
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  /** The full row of a set (the edit dialog's seed). */
  loadSet(name: string): Promise<AdminSetRow | undefined> {
    return this.run(() => getSet(name));
  }

  /** PATCH description + member order in one request, then re-read. */
  async saveSet(
    name: string,
    body: { description?: string; members?: string[] },
  ): Promise<void> {
    await this.run(async () => {
      await updateSet(name, body);
      await this.refreshAll();
    });
  }
  /** Drop a parked profile into a set: re-read the set's live member
   * list first (the drop appends to what the server has, not a stale
   * cache), treat an id that is already inside as a no-op drop, and
   * put the full ordered list back. The server adopts a member that
   * still parks in the caller's space: the membership rewrite
   * moves it into the catalog in the same transaction.
   */
  async addParkedToSet(profileId: string, setName: string): Promise<boolean> {
    const row = await this.loadSet(setName);
    if (!row) return false;
    const members = row.profiles.map((p) => p.profile_id);
    if (members.includes(profileId)) return true;
    const ok = await this.run(async () => {
      await updateSet(setName, { members: [...members, profileId] });
      await this.refreshAll();
      return true;
    });
    return ok === true;
  }

  async removeSet(name: string): Promise<void> {
    await this.run(async () => {
      await deleteSet(name);
      await this.refreshAll();
    });
  }

  /** PATCH the row's shape (the credential rides the same request). */
  async saveProvider(
    providerId: string,
    body: {
      family?: string;
      base_url?: string | null;
      api_key?: string | null;
    },
  ): Promise<boolean> {
    const ok = await this.run(() => updateProvider(providerId, body));
    if (ok === undefined) return false;
    // The refresh failure surfaces on the page-level alert; the
    // action itself landed, so the caller closes on true.
    await this.refreshAll();
    return true;
  }

  async removeProvider(providerId: string): Promise<void> {
    await this.run(async () => {
      await deleteProvider(providerId);
      await this.refreshAll();
    });
  }

  /** Re-read just the parking slice (the catalog space's drafts). */
  async refreshParking(): Promise<void> {
    const body = await this.run(() => listParking());
    if (body) this.parking = body;
  }

  /** Create a parked draft, then re-read the parking slice. */
  async addParked(body: {
    profile_id: string;
    provider_id: string;
    model: string;
    max_context_window?: number | null;
    effort?: string | null;
    modalities?: string[] | null;
    max_budget?: number | null;
    tpm_limit?: number | null;
    rpm_limit?: number | null;
  }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createParked(body);
      await this.refreshParking();
      return true;
    });
    return ok === true;
  }

  /** PUT the parked row's shape, then re-read the parking slice. */
  async saveParked(
    profileId: string,
    body: {
      model: string;
      max_context_window?: number | null;
      effort?: string | null;
      modalities?: string[] | null;
      parked: boolean;
      max_budget?: number | null;
      tpm_limit?: number | null;
      rpm_limit?: number | null;
    },
  ): Promise<boolean> {
    const ok = await this.run(() => updateParked(profileId, body));
    if (ok === undefined) return false;
    await this.refreshParking();
    return true;
  }

  /** Delete a parked draft, then re-read the parking slice. */
  async removeParked(profileId: string): Promise<void> {
    await this.run(async () => {
      await deleteParked(profileId);
      await this.refreshParking();
    });
  }

  /** Register a catalog set, then re-read. Answers whether the
   * create landed (the refresh failure rides the page-level alert). */
  async addSet(
    collection: string,
    name: string,
    description: string,
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await createSet(collection, { name, description });
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }
  /** Transfer a collection's default-set anchor to a member set, then
   * re-read. Answers whether the transfer landed. */
  async setCollectionDefault(
    collection: string,
    setName: string,
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await setCollectionDefault(collection, setName);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  /** PUT a provider credential, then re-read (the mask).
   * Answers whether the PUT landed (the refresh rides separately). */
  async saveCredential(
    providerId: string,
    baseUrl: string,
    apiKey: string,
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await putProviderCredential(providerId, baseUrl, apiKey);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  /** Delete a provider credential, then re-read. Answers
   * whether the delete landed (the refresh rides separately). */
  async removeCredential(providerId: string): Promise<boolean> {
    const ok = await this.run(async () => {
      await deleteProviderCredential(providerId);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }
  /** Register a catalog collection, then re-read. Answers whether the
   * create landed (the refresh failure rides the page-level alert). */
  async addCollection(body: {
    name: string;
    description: string;
  }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createCollection(body);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  /** PATCH the description, then re-read.
   * Answers whether the PATCH landed (the refresh failure rides the
   * page-level alert). */
  async saveCollection(
    name: string,
    body: { description?: string },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateCollection(name, body);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  async removeCollection(name: string): Promise<void> {
    await this.run(async () => {
      await deleteCollection(name);
      await this.refreshAll();
    });
  }

  /** Publish the collection to one platform audience, then re-read. */
  async publishToGroup(name: string, groupId: string): Promise<void> {
    await this.run(async () => {
      await publishCollection(name, groupId);
      await this.refreshAll();
    });
  }

  /** Withdraw one platform audience, then re-read. */
  async unpublishFromGroup(name: string, groupId: string): Promise<void> {
    await this.run(async () => {
      await unpublishCollection(name, groupId);
      await this.refreshAll();
    });
  }

  /** Register a platform group, then re-read. Answers whether the
   * create landed (the refresh failure rides the page-level alert). */
  async addGroup(body: { name: string; members?: string[] }): Promise<boolean> {
    const ok = await this.run(async () => {
      await createGroup(body);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  /** PATCH name and/or membership, then re-read. Answers whether the
   * PATCH landed (the refresh failure rides the page-level alert). */
  async saveGroup(
    groupId: string,
    body: { name?: string; members?: string[] },
  ): Promise<boolean> {
    const ok = await this.run(async () => {
      await updateGroup(groupId, body);
      return true;
    });
    if (ok !== true) return false;
    await this.refreshAll();
    return true;
  }

  async removeGroup(groupId: string): Promise<void> {
    await this.run(async () => {
      await deleteGroup(groupId);
      await this.refreshAll();
    });
  }

  /** Add one account to a group, then re-read. */
  async addMember(groupId: string, account: string): Promise<void> {
    await this.run(async () => {
      await addGroupMember(groupId, account);
      await this.refreshAll();
    });
  }

  /** Remove one account from a group, then re-read. */
  async removeMember(groupId: string, account: string): Promise<void> {
    await this.run(async () => {
      await removeGroupMember(groupId, account);
      await this.refreshAll();
    });
  }
}

export const gatewayStore = new GatewayAdminStore();
