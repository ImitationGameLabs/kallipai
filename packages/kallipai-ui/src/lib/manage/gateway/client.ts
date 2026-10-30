/**
 * The gateway admin face client. The admin base URL comes from the baked
 * runtime config (`gatewayAdminUrl`); every request rides the operator's
 * archeion session cookie (credentials: include), and mutating requests
 * carry the CSRF marker the gateway's csrf_guard checks. No credential
 * is stored client-side.
 */

/** The CSRF marker the gateway's csrf_guard checks on mutating requests. */
const CSRF_HEADER = "X-Requested-With";
const CSRF_HEADER_VALUE = "kallipai";

/** The baked admin base URL, or "" when the deployment bakes none in. */
export function bakedAdminUrl(): string {
  const baked = (
    globalThis as unknown as { KALLIPAI_CONFIG?: { gatewayAdminUrl?: unknown } }
  ).KALLIPAI_CONFIG?.gatewayAdminUrl;
  return typeof baked === "string" ? baked : "";
}

export function isConnected(url = bakedAdminUrl()): boolean {
  return url.trim() !== "";
}

// -- the provider pool ----------------------------------------------------
// Wire shapes verbatim from the server (management/routes.rs): the list
// wraps in ProviderViews, ProviderView carries the mask only (the
// plaintext has no path out of this face), and the credential PUT body
// names the key field upstream_api_key.
export interface ProviderRow {
  owner: string;
  provider_id: string;
  family: string;
  base_url: string | null;
  api_key_masked: string | null;
}

export function listProviders(url?: string): Promise<ProviderRow[]> {
  return adminFetch<{ providers: ProviderRow[] }>("/admin/providers", {
    url,
  }).then((r) => r.providers);
}

export function createProvider(
  body: {
    provider_id: string;
    family: string;
    base_url?: string | null;
    api_key?: string | null;
  },
  url?: string,
): Promise<ProviderRow> {
  return adminFetch<ProviderRow>("/admin/providers", {
    method: "POST",
    body,
    url,
  });
}

export function updateProvider(
  providerId: string,
  body: {
    family?: string;
    base_url?: string | null;
    api_key?: string | null;
  },
  url?: string,
): Promise<ProviderRow> {
  return adminFetch<ProviderRow>(
    `/admin/providers/${encodeURIComponent(providerId)}`,
    { method: "PATCH", body, url },
  );
}

export function deleteProvider(
  providerId: string,
  url?: string,
): Promise<void> {
  return adminFetch(`/admin/providers/${encodeURIComponent(providerId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

export function putProviderCredential(
  providerId: string,
  upstreamBaseUrl: string,
  upstreamApiKey: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/providers/${encodeURIComponent(providerId)}/credential`,
    {
      method: "PUT",
      body: {
        upstream_base_url: upstreamBaseUrl,
        upstream_api_key: upstreamApiKey,
      },
      url,
    },
  ).then(() => undefined);
}

export function deleteProviderCredential(
  providerId: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/providers/${encodeURIComponent(providerId)}/credential`,
    { method: "DELETE", url },
  ).then(() => undefined);
}

export class AdminError extends Error {
  readonly status: number;
  /** The server's human-readable reason (the error envelope's message). */
  readonly serverMessage: string | null;

  constructor(status: number, serverMessage: string | null) {
    super(
      `gateway admin ${status}${serverMessage ? `: ${serverMessage}` : ""}`,
    );
    this.status = status;
    this.serverMessage = serverMessage;
  }
}

/** One admin-face request: the session cookie rides along, mutating
 * requests carry the CSRF marker, JSON in and out, ApiError mapping. */
export async function adminFetch<T>(
  path: string,
  init: { method?: string; body?: unknown; url?: string } = {},
): Promise<T> {
  const base = (init.url ?? bakedAdminUrl()).replace(/\/+$/, "");
  const method = init.method ?? "GET";
  const headers: Record<string, string> = { accept: "application/json" };
  if (method !== "GET") {
    headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
    if (init.body !== undefined) headers["content-type"] = "application/json";
  }
  const response = await fetch(`${base}${path}`, {
    method,
    headers,
    credentials: "include",
    body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
  });
  if (!response.ok) {
    let serverMessage: string | null = null;
    try {
      const parsed = (await response.json()) as {
        error?: { message?: string };
      };
      serverMessage = parsed?.error?.message ?? null;
    } catch {
      // Non-JSON error body: the status alone carries the signal.
    }
    throw new AdminError(response.status, serverMessage);
  }
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

// -- sets / profiles ----------------------------------------------------
// The shapes mirror the server's admin views (management/sets.rs and
// profiles.rs): every set and profile address names its collection in
// the path, and each row echoes the container back as collection_name.
// The one cross-collection read is the collection-less profiles list.
// The budget trio is absent from the admin views: the server sends no
// such fields, so they read as undefined at runtime.
export interface AdminProfileRow {
  profile_id: string;
  /** The collection this row belongs to (part of its identity). */
  collection_name: string;
  provider_id: string;
  /** The ownership reservation: `system` is the catalog. */
  owner: string;
  family: string;
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[];
  /** The set that carries this row; null leaves it unassigned. */
  set: string | null;
  /** Rank inside that set (0 while unassigned). */
  position: number;
  /** Absent from the wire: undefined at runtime. */
  max_budget: number | null;
  tpm_limit: number | null;
  rpm_limit: number | null;
}

export interface AdminSetRow {
  /** The collection this set is anchored to (part of its identity). */
  collection_name: string;
  name: string;
  description: string;
  profiles: AdminProfileRow[];
}

export function listCollectionSets(
  collection: string,
  url?: string,
): Promise<AdminSetRow[]> {
  return adminFetch<{ sets: AdminSetRow[] }>(
    `/admin/collections/${encodeURIComponent(collection)}/sets`,
    { url },
  ).then((r) => r.sets);
}

/** GET /admin/collections/{collection}/sets/{name}: one set under the
 * collection that anchors it. */
export function getCollectionSet(
  collection: string,
  name: string,
  url?: string,
): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(
    `/admin/collections/${encodeURIComponent(collection)}/sets/${encodeURIComponent(name)}`,
    { url },
  );
}

/** POST /admin/collections/{collection}/sets: a set born in the named
 * collection. The same name in another collection is a different set. */
export function createCollectionSet(
  collection: string,
  body: { name: string; description: string },
  url?: string,
): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(
    `/admin/collections/${encodeURIComponent(collection)}/sets`,
    {
      method: "POST",
      body,
      url,
    },
  );
}

/** PATCH the set's description; membership and order have their own writes. */
export function updateCollectionSet(
  collection: string,
  name: string,
  body: { description?: string },
  url?: string,
): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(
    `/admin/collections/${encodeURIComponent(collection)}/sets/${encodeURIComponent(name)}`,
    {
      method: "PATCH",
      body,
      url,
    },
  );
}

/** PUT the set's member order. The body is the complete membership in the
 * new order, so it has to be a permutation of what the set holds: the server
 * refuses a list that adds or drops a member. */
export function reorderCollectionSet(
  collection: string,
  name: string,
  members: readonly string[],
  url?: string,
): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(
    `/admin/collections/${encodeURIComponent(collection)}/sets/${encodeURIComponent(name)}/order`,
    {
      method: "PUT",
      body: { members },
      url,
    },
  );
}

export function deleteCollectionSet(
  collection: string,
  name: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/collections/${encodeURIComponent(collection)}/sets/${encodeURIComponent(name)}`,
    { method: "DELETE", url },
  ).then(() => undefined);
}

/** The catalog space's unassigned profiles across every collection:
 * the `unassigned` query flag over the profiles list. */
export function listUnassigned(url?: string): Promise<AdminProfileRow[]> {
  return listProfiles({ unassigned: true }, url);
}

/** GET /admin/profiles: the catalog space's rows, filtered in the query.
 * A collection-less read answers every collection of the space, so the
 * cross-collection panels keep one door. */
export function listProfiles(
  filter: { collection_name?: string; unassigned?: boolean } = {},
  url?: string,
): Promise<AdminProfileRow[]> {
  const parts: string[] = [];
  if (filter.collection_name !== undefined) {
    parts.push(`collection_name=${encodeURIComponent(filter.collection_name)}`);
  }
  if (filter.unassigned !== undefined)
    parts.push(`unassigned=${filter.unassigned}`);
  const suffix = parts.length === 0 ? "" : `?${parts.join("&")}`;
  return adminFetch<AdminProfileRow[]>(`/admin/profiles${suffix}`, { url });
}

/** POST /admin/collections/{collection}/profiles: a row born in the named
 * collection. Only a same-id row in that collection conflicts; the same id
 * in another collection is a different row. The server validates the
 * provider reference and derives the family from the provider row. */
export function createProfile(
  collection: string,
  body: {
    profile_id: string;
    provider_id: string;
    model: string;
    max_context_window?: number | null;
    effort?: string | null;
    modalities?: string[] | null;
  },
  url?: string,
): Promise<AdminProfileRow> {
  return adminFetch<AdminProfileRow>(
    `/admin/collections/${encodeURIComponent(collection)}/profiles`,
    { method: "POST", body, url },
  );
}

/** PUT the row's full content in place (plain PUT semantics: the body is
 * the complete desired state). `set` is the set that carries the row, or
 * null to leave it unassigned; a name joins that set at its last place. */
export function updateProfile(
  collection: string,
  profileId: string,
  body: {
    model: string;
    max_context_window?: number | null;
    effort?: string | null;
    modalities?: string[] | null;
    set: string | null;
  },
  url?: string,
): Promise<AdminProfileRow> {
  return adminFetch<AdminProfileRow>(
    `/admin/collections/${encodeURIComponent(collection)}/profiles/${encodeURIComponent(profileId)}`,
    { method: "PUT", body, url },
  );
}

export function deleteProfile(
  collection: string,
  profileId: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/collections/${encodeURIComponent(collection)}/profiles/${encodeURIComponent(profileId)}`,
    { method: "DELETE", url },
  ).then(() => undefined);
}

/** POST /admin/collections/{target}/profiles/{id}/move: re-anchor one row.
 * `from` names the source collection; `set` (which must live in the
 * target) makes the arrival a served member instead of an unassigned row. The
 * row leaves and the copy is the same row, id and content included. */
export function moveProfile(
  targetCollection: string,
  fromCollection: string,
  profileId: string,
  set?: string,
  url?: string,
): Promise<AdminProfileRow> {
  return adminFetch<AdminProfileRow>(
    `/admin/collections/${encodeURIComponent(targetCollection)}/profiles/${encodeURIComponent(profileId)}/move`,
    {
      method: "POST",
      body: { from_collection_name: fromCollection, set: set ?? null },
      url,
    },
  );
}

// -- collections / groups (the reach face) --------------------------------
// Wire shapes verbatim from the server (management/routes.rs): the
// admin collection view carries the live publications (the platform
// audiences), which the user face's shape does not.

export interface AdminCollectionRow {
  name: string;
  description: string;
  sets: string[];
  /** The default-set anchor (null = none chosen yet). */
  default_set: string | null;
  /** The platform audiences the collection is live on. */
  publications: string[];
  /** How many profile rows the collection holds (unassigned included). */
  profile_count: number;
  /** How many sets the collection anchors. */
  set_count: number;
}

export function listCollections(url?: string): Promise<AdminCollectionRow[]> {
  return adminFetch<{ collections: AdminCollectionRow[] }>(
    "/admin/collections",
    {
      url,
    },
  ).then((r) => r.collections);
}

export function getCollection(
  name: string,
  url?: string,
): Promise<AdminCollectionRow> {
  return adminFetch(`/admin/collections/${encodeURIComponent(name)}`, { url });
}

export function createCollection(
  body: { name: string; description: string },
  url?: string,
): Promise<AdminCollectionRow> {
  return adminFetch<AdminCollectionRow>("/admin/collections", {
    method: "POST",
    body,
    url,
  });
}

export function updateCollection(
  name: string,
  body: { description?: string },
  url?: string,
): Promise<AdminCollectionRow> {
  return adminFetch<AdminCollectionRow>(
    `/admin/collections/${encodeURIComponent(name)}`,
    { method: "PATCH", body, url },
  );
}

export function deleteCollection(name: string, url?: string): Promise<void> {
  return adminFetch(`/admin/collections/${encodeURIComponent(name)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

/** Transfer the collection's default-set anchor to a member set. */
export function setCollectionDefault(
  name: string,
  setName: string,
  url?: string,
): Promise<void> {
  return adminFetch(`/admin/collections/${encodeURIComponent(name)}/default`, {
    method: "PATCH",
    body: { set_name: setName },
    url,
  }).then(() => undefined);
}
export function publishCollection(
  name: string,
  groupId: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/collections/${encodeURIComponent(name)}/publications`,
    {
      method: "POST",
      body: { audience: groupId },
      url,
    },
  ).then(() => undefined);
}

export function unpublishCollection(
  name: string,
  groupId: string,
  url?: string,
): Promise<void> {
  return adminFetch(
    `/admin/collections/${encodeURIComponent(name)}/publications/${encodeURIComponent(groupId)}`,
    {
      method: "DELETE",
      url,
    },
  ).then(() => undefined);
}

export interface GroupRow {
  group_id: string;
  name: string;
  members: string[];
}

export function listGroups(url?: string): Promise<GroupRow[]> {
  return adminFetch<{ groups: GroupRow[] }>("/admin/groups", {
    url,
  }).then((r) => r.groups);
}

export function createGroup(
  body: { name: string; members?: string[] },
  url?: string,
): Promise<GroupRow> {
  return adminFetch<GroupRow>("/admin/groups", {
    method: "POST",
    body,
    url,
  });
}

export function updateGroup(
  groupId: string,
  body: { name?: string; members?: string[] },
  url?: string,
): Promise<GroupRow> {
  return adminFetch<GroupRow>(`/admin/groups/${encodeURIComponent(groupId)}`, {
    method: "PATCH",
    body,
    url,
  });
}

export function deleteGroup(groupId: string, url?: string): Promise<void> {
  return adminFetch(`/admin/groups/${encodeURIComponent(groupId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

export function addGroupMember(
  groupId: string,
  memberAccount: string,
  url?: string,
): Promise<GroupRow> {
  return adminFetch<GroupRow>(
    `/admin/groups/${encodeURIComponent(groupId)}/members`,
    {
      method: "POST",
      body: { member_account: memberAccount },
      url,
    },
  );
}

export function removeGroupMember(
  groupId: string,
  memberAccount: string,
  url?: string,
): Promise<GroupRow> {
  return adminFetch<GroupRow>(
    `/admin/groups/${encodeURIComponent(groupId)}/members/${encodeURIComponent(
      memberAccount,
    )}`,
    { method: "DELETE", url },
  );
}

export interface AccountSearchRow {
  account_id: string;
  username: string;
  /** The display name, or null when the account carries none. */
  display_name: string | null;
  disabled: boolean;
}

export function searchAccounts(
  query: string,
  url?: string,
): Promise<AccountSearchRow[]> {
  return adminFetch<{ users: AccountSearchRow[] }>("/admin/accounts/search", {
    method: "POST",
    body: { query },
    url,
  }).then((r) => r.users);
}
