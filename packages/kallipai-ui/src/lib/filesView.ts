// Pure client-side view model for the files page: strip the
// caller's /users/{me}/ prefix, then group by the first user-space segment.
// The server returns space_path ASC with no cursor, so groups keep the
// server order via first appearance. Label resolution for tagma groups
// (enrolled store) stays with the caller, keeping this module store-free.
import type { FileEntryView } from "@kallipai/kallipai-files-client";

/** One display row: the server entry plus its user-space-relative path. */
export interface FileRowView {
  id: string;
  /** Full space path, e.g. /users/u1/shared/report.pdf. */
  path: string;
  /** The path relative to the caller's space prefix (what the UI shows). */
  displayPath: string;
  size: number;
  createdAt: string;
}

/** A first-segment group of rows. The upload prefix is free-form, so
 * beyond the three named families an entry can sit at the space root
 * ("root": uploaded with an empty prefix) or under a user-named folder
 * ("folder": rendered under its own segment name). */
export interface FileGroupView {
  kind: "inbox" | "shared" | "tagma" | "root" | "folder";
  /** The tagma id for kind "tagma" (not resolved to a label here). */
  tagmaId?: string;
  /** The segment name for kind "folder". */
  label?: string;
  rows: FileRowView[];
}

/** The user-space-relative display path: strip the caller's prefix, with
 * a defensive leading-slash strip for a path outside it (it still
 * renders rather than vanishing). Shared by the grouping and the filter. */
export function displayPath(path: string, selfPrefix: string): string {
  return path.startsWith(selfPrefix)
    ? path.slice(selfPrefix.length)
    : path.replace(/^\//, "");
}
/** Group the caller's entries by their first user-space segment. */
export function groupFileEntries(
  entries: FileEntryView[],
  selfPrefix: string,
): FileGroupView[] {
  const groups: FileGroupView[] = [];
  const byKey = new Map<string, FileGroupView>();
  for (const entry of entries) {
    const rel = displayPath(entry.path, selfPrefix);
    const slash = rel.indexOf("/");
    const head = slash === -1 ? rel : rel.slice(0, slash);
    const rest = slash === -1 ? "" : rel.slice(slash + 1);
    const tagmaId =
      head === "tagmas" && rest.length > 0 ? rest.split("/")[0] : undefined;
    const kind: FileGroupView["kind"] =
      tagmaId !== undefined
        ? "tagma"
        : slash === -1
          ? "root"
          : head === "inbox" || head === "shared"
            ? head
            : "folder";
    const key =
      kind === "tagma" ? `tagmas/${tagmaId}` : kind === "folder" ? head : kind;
    let group = byKey.get(key);
    if (!group) {
      group =
        kind === "tagma"
          ? { kind, tagmaId, rows: [] }
          : kind === "folder"
            ? { kind, label: head, rows: [] }
            : { kind, rows: [] };
      byKey.set(key, group);
      groups.push(group);
    }
    group.rows.push({
      id: entry.id,
      path: entry.path,
      displayPath: rel,
      size: entry.size,
      createdAt: entry.created_at,
    });
  }
  return groups;
}

/** Case-insensitive prefix match from the start of the display path:
 * a string prefix filter, not a substring search -- "rep" matches
 * report.pdf but not shared/report.pdf). An empty query matches all. */
export function matchesFileFilter(displayPath: string, query: string): boolean {
  return displayPath.toLowerCase().startsWith(query.trim().toLowerCase());
}
