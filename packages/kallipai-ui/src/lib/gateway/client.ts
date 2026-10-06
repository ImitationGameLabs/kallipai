/**
 * The gateway user face client (the /user routes behind the session
 * cookie). The base URL is the same baked gateway origin the admin
 * client reads: one axum service serves both faces, so the config key
 * predates this client and no second URL exists. Every request rides
 * the platform session cookie (credentials: include), mutating
 * requests carry the CSRF marker the gateway's csrf_guard checks, and
 * no provider credential is stored client-side.
 */

/** The CSRF marker the gateway's csrf_guard checks on mutating requests. */
const CSRF_HEADER = "X-Requested-With";
const CSRF_HEADER_VALUE = "kallipai";

/** The reserved audience id: the built-in group every account belongs to. */
export const EVERYONE_GROUP_ID = "everyone";

/** The baked gateway base URL, or "" when unset (the page asks for it). */
export function bakedGatewayUrl(): string {
  const baked = (
    globalThis as unknown as { KALLIPAI_CONFIG?: { gatewayAdminUrl?: unknown } }
  ).KALLIPAI_CONFIG?.gatewayAdminUrl;
  return typeof baked === "string" ? baked : "";
}

/**
 * The client-side credential-URL predicate, the same one the server
 * applies: a value `new URL()` parses is absolute, anything else is
 * refused before the request leaves the page.
 */
export function isAbsoluteUrl(value: string): boolean {
  try {
    new URL(value);
    return true;
  } catch {
    return false;
  }
}

export class UserGatewayError extends Error {
  readonly status: number;
  /** The server's human-readable reason (the error envelope's message). */
  readonly serverMessage: string | null;

  constructor(status: number, serverMessage: string | null) {
    super(`gateway user ${status}${serverMessage ? `: ${serverMessage}` : ""}`);
    this.status = status;
    this.serverMessage = serverMessage;
  }
}

/** One user-face request: the session cookie rides along, mutating
 * requests carry the CSRF marker, JSON in and out, ApiError mapping. */
export async function userFetch<T>(
  path: string,
  init: { method?: string; body?: unknown; url?: string } = {},
): Promise<T> {
  const base = (init.url ?? bakedGatewayUrl()).replace(/\/+$/, "");
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
    throw new UserGatewayError(response.status, serverMessage);
  }
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

// -- wire shapes -----------------------------------------------------------
// Verbatim from the server (management/user_routes.rs): the owner is
// always the caller and is never echoed; the key appears only masked.

export interface UserProviderRow {
  provider_id: string;
  family: string;
  base_url: string | null;
  api_key_masked: string | null;
}

export interface UserProfileRow {
  profile_id: string;
  provider_id: string;
  family: string;
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[] | null;
  max_budget: number | null;
  tpm_limit: number | null;
  rpm_limit: number | null;
  parked: boolean;
}

export interface UserSetRow {
  name: string;
  description: string;
  /** The failover order (the member list order is the sequence). */
  members: string[];
}

export interface UserCollectionRow {
  name: string;
  description: string;
  sets: string[];
}

export interface UserGroupRow {
  group_id: string;
  name: string;
  members: string[];
}

/** One platform-published collection the caller's platform reach
 * carries (read-only; the catalog is platform material). */
export interface UserPlatformCollectionRow {
  owner: string;
  name: string;
  description: string;
  sets: string[];
}

// -- providers --------------------------------------------------------------

export function listProviders(url?: string): Promise<UserProviderRow[]> {
  return userFetch<{ providers: UserProviderRow[] }>("/user/providers", {
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
): Promise<UserProviderRow> {
  return userFetch<UserProviderRow>("/user/providers", {
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
): Promise<UserProviderRow> {
  return userFetch<UserProviderRow>(
    `/user/providers/${encodeURIComponent(providerId)}`,
    { method: "PATCH", body, url },
  );
}

export function deleteProvider(
  providerId: string,
  url?: string,
): Promise<void> {
  return userFetch(`/user/providers/${encodeURIComponent(providerId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

export function putProviderCredential(
  providerId: string,
  upstreamBaseUrl: string,
  upstreamApiKey: string,
  url?: string,
): Promise<UserProviderRow> {
  return userFetch<UserProviderRow>(
    `/user/providers/${encodeURIComponent(providerId)}/credential`,
    {
      method: "PUT",
      body: {
        upstream_base_url: upstreamBaseUrl,
        upstream_api_key: upstreamApiKey,
      },
      url,
    },
  );
}

export function deleteProviderCredential(
  providerId: string,
  url?: string,
): Promise<void> {
  return userFetch(
    `/user/providers/${encodeURIComponent(providerId)}/credential`,
    { method: "DELETE", url },
  ).then(() => undefined);
}

// -- profiles ---------------------------------------------------------------

export function listProfiles(url?: string): Promise<UserProfileRow[]> {
  return userFetch<{ profiles: UserProfileRow[] }>("/user/profiles", {
    url,
  }).then((r) => r.profiles);
}

export function createProfile(
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
): Promise<UserProfileRow> {
  return userFetch<UserProfileRow>("/user/profiles", {
    method: "POST",
    body,
    url,
  });
}

/** PUT is a full-field replacement (parked is not caller-settable). */
export function updateProfile(
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
  url?: string,
): Promise<UserProfileRow> {
  return userFetch<UserProfileRow>(
    `/user/profiles/${encodeURIComponent(profileId)}`,
    { method: "PUT", body, url },
  );
}

export function deleteProfile(profileId: string, url?: string): Promise<void> {
  return userFetch(`/user/profiles/${encodeURIComponent(profileId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

// -- sets -------------------------------------------------------------------

export function listSets(url?: string): Promise<UserSetRow[]> {
  return userFetch<{ sets: UserSetRow[] }>("/user/sets", { url }).then(
    (r) => r.sets,
  );
}

export function getSet(name: string, url?: string): Promise<UserSetRow> {
  return userFetch<UserSetRow>(`/user/sets/${encodeURIComponent(name)}`, {
    url,
  });
}

export function createSet(
  collection: string,
  body: { name: string; description: string; members?: string[] },
  url?: string,
): Promise<UserSetRow> {
  return userFetch<UserSetRow>(
    `/user/collections/${encodeURIComponent(collection)}/sets`,
    { method: "POST", body, url },
  );
}

export function updateSet(
  name: string,
  body: { description?: string; members?: string[] },
  url?: string,
): Promise<UserSetRow> {
  return userFetch<UserSetRow>(`/user/sets/${encodeURIComponent(name)}`, {
    method: "PATCH",
    body,
    url,
  });
}

export function deleteSet(name: string, url?: string): Promise<void> {
  return userFetch(`/user/sets/${encodeURIComponent(name)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

// -- collections ------------------------------------------------------------

export function listCollections(url?: string): Promise<UserCollectionRow[]> {
  return userFetch<{ collections: UserCollectionRow[] }>("/user/collections", {
    url,
  }).then((r) => r.collections);
}

export function getCollection(
  name: string,
  url?: string,
): Promise<UserCollectionRow> {
  return userFetch<UserCollectionRow>(
    `/user/collections/${encodeURIComponent(name)}`,
    { url },
  );
}

export function createCollection(
  body: { name: string; description: string; sets?: string[] },
  url?: string,
): Promise<UserCollectionRow> {
  return userFetch<UserCollectionRow>("/user/collections", {
    method: "POST",
    body,
    url,
  });
}

export function updateCollection(
  name: string,
  body: { description?: string; sets?: string[] },
  url?: string,
): Promise<UserCollectionRow> {
  return userFetch<UserCollectionRow>(
    `/user/collections/${encodeURIComponent(name)}`,
    { method: "PATCH", body, url },
  );
}

export function deleteCollection(name: string, url?: string): Promise<void> {
  return userFetch(`/user/collections/${encodeURIComponent(name)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

export function publishCollection(
  name: string,
  groupId: string,
  url?: string,
): Promise<void> {
  return userFetch(
    `/user/collections/${encodeURIComponent(name)}/publications`,
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
  return userFetch(
    `/user/collections/${encodeURIComponent(name)}/publications/${encodeURIComponent(groupId)}`,
    {
      method: "DELETE",
      url,
    },
  ).then(() => undefined);
}

// -- groups -----------------------------------------------------------------

export function listGroups(url?: string): Promise<UserGroupRow[]> {
  return userFetch<{ groups: UserGroupRow[] }>("/user/groups", {
    url,
  }).then((r) => r.groups);
}

export function getGroup(groupId: string, url?: string): Promise<UserGroupRow> {
  return userFetch<UserGroupRow>(
    `/user/groups/${encodeURIComponent(groupId)}`,
    { url },
  );
}

export function createGroup(
  body: { name: string; members?: string[] },
  url?: string,
): Promise<UserGroupRow> {
  return userFetch<UserGroupRow>("/user/groups", { method: "POST", body, url });
}

export function updateGroup(
  groupId: string,
  body: { name?: string; members?: string[] },
  url?: string,
): Promise<UserGroupRow> {
  return userFetch<UserGroupRow>(
    `/user/groups/${encodeURIComponent(groupId)}`,
    { method: "PATCH", body, url },
  );
}

export function deleteGroup(groupId: string, url?: string): Promise<void> {
  return userFetch(`/user/groups/${encodeURIComponent(groupId)}`, {
    method: "DELETE",
    url,
  }).then(() => undefined);
}

// -- the platform catalog ---------------------------------------------------

export function listPlatformCollections(
  url?: string,
): Promise<UserPlatformCollectionRow[]> {
  return userFetch<{ collections: UserPlatformCollectionRow[] }>(
    "/user/platform-collections",
    { url },
  ).then((r) => r.collections);
}
