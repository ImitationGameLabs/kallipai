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

/** The global files page: `/files` (the user-scope file manager). */
export function filesPath(): string {
  return "/files";
}
