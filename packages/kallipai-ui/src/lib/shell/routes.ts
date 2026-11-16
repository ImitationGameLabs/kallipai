/**
 * Single source for the app's URLs (the path-builder; tagma-centric at
 * first; global pages like /files live here too).
 * a future route rename is a one-line change instead of a repo-wide string
 * hunt. Route params are interpolated verbatim; callers pass real tagma ids.
 */

/** The tagma chat page: `/tagma/<uuid>/chat`. */
export function tagmaChatPath(tagmaId: string): string {
  return `/tagma/${tagmaId}/chat`;
}

/** The manage details hub: `/tagma/<uuid>/details`; the hub route is a
 * permanent redirect to the overview section. */
export function tagmaDetailsPath(tagmaId: string): string {
  return `/tagma/${tagmaId}/details`;
}

/** The manage details sections beneath the hub. */
export type TagmaDetailsSection =
  | "overview"
  | "budget"
  | "agents"
  | "profiles"
  | "schedules"
  | "tasks";

/** A manage details section: `/tagma/<uuid>/details/<section>`. */
export function tagmaDetailsSectionPath(
  tagmaId: string,
  section: TagmaDetailsSection,
): string {
  return `${tagmaDetailsPath(tagmaId)}/${section}`;
}

/** An agent detail page: `/tagma/<uuid>/details/agents/<agentId>`. */
export function tagmaAgentPath(tagmaId: string, agentId: string): string {
  return `${tagmaDetailsSectionPath(tagmaId, "agents")}/${agentId}`;
}

/** An agent's turn history beneath a manage base path: `/tagma/<uuid>/details`
 * online, `/local/manage` offline, one shape for both host trees. */
export function agentHistoryPath(basePath: string, agentId: string): string {
  return `${basePath}/agents/${agentId}/history`;
}

/** The global files page: `/files` (the user-scope file manager). */
export function filesPath(): string {
  return "/files";
}

/** The gateway admin console: `/admin/gateway` (the platform catalog face). */
export function adminGatewayPath(): string {
  return "/admin/gateway";
}

/** A gateway collection's detail page: `/admin/gateway/collections/<name>`. */
export function adminGatewayCollectionPath(name: string): string {
  return `/admin/gateway/collections/${encodeURIComponent(name)}`;
}

/** The caller's gateway space: `/gateway` (the user face). */
export function gatewayPath(): string {
  return "/gateway";
}

/** A room's transcript: `/rooms/<id>`, optionally landing on a row. */
export function roomPath(
  roomId: string,
  options: { at?: number } = {},
): string {
  const path = `/rooms/${roomId}`;
  return options.at === undefined ? path : `${path}?at=${options.at}`;
}

/** One discussion's own page: `/rooms/<id>/threads/<root>`.
 *  `at` lands the read on a reply; `quote` hands the page a row to answer. */
export function roomThreadPath(
  roomId: string,
  root: number,
  options: { at?: number; quote?: number } = {},
): string {
  const path = `/rooms/${roomId}/threads/${root}`;
  const query = new URLSearchParams();
  if (options.at !== undefined) query.set("at", String(options.at));
  if (options.quote !== undefined) {
    query.set("quote", String(options.quote));
  }
  const search = query.toString();
  return search === "" ? path : `${path}?${search}`;
}
