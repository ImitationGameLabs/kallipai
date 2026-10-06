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

// -- sets / parking ----------------------------------------------------
// Wire shapes verbatim from the server (management/routes.rs): the
// nested collection list wraps AdminSet rows, list_parked answers a
// bare AdminProfile[] (the one unwrap-less GET in the family).
export interface AdminProfileRow {
  profile_id: string;
  provider_id: string;
  /** The ownership reservation: `system` is the catalog. */
  owner: string;
  family: string;
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[];
  parked: boolean;
  max_budget: number | null;
  tpm_limit: number | null;
  rpm_limit: number | null;
}

export interface AdminSetRow {
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

export function getSet(name: string, url?: string): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(`/admin/sets/${encodeURIComponent(name)}`, {
    url,
  });
}

export function createSet(
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

export function updateSet(
  name: string,
  body: { description?: string; members?: string[] },
  url?: string,
): Promise<AdminSetRow> {
  return adminFetch<AdminSetRow>(`/admin/sets/${encodeURIComponent(name)}`, {
    method: "PATCH",
    body,
    url,
  });
}

export function deleteSet(name: string, url?: string): Promise<void> {
  return adminFetch(`/admin/sets/${encodeURIComponent(name)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

export function listParking(url?: string): Promise<AdminProfileRow[]> {
  return adminFetch<AdminProfileRow[]>(`/admin/parking`, { url });
}

/** Create a parked draft referencing a provider; the server validates
 * the reference and derives the family from the provider row. */
export function createParked(
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
  url?: string,
): Promise<AdminProfileRow> {
  return adminFetch<AdminProfileRow>("/admin/parking", {
    method: "POST",
    body,
    url,
  });
}

/** Full-field replacement of a parked draft (plain PUT semantics). The
 * family and profile id are immutable here; `parked` moves the row
 * between the parking area and the served face. */
export function updateParked(
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
  url?: string,
): Promise<AdminProfileRow> {
  return adminFetch<AdminProfileRow>(
    `/admin/parking/${encodeURIComponent(profileId)}`,
    { method: "PUT", body, url },
  );
}

export function deleteParked(profileId: string, url?: string): Promise<void> {
  return adminFetch(`/admin/parking/${encodeURIComponent(profileId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
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
