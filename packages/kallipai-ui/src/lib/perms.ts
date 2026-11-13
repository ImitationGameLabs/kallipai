// The permission vocabulary the servers check, in the order a picker renders
// it. One flat `surface:resource:action` string per capability, mirrored from
// `crates/platform/kallipai-archeion-common/src/perm.rs`. The ids are wire
// values: they must match the server byte for byte, so nothing here renames
// them. The labels are the human face and live in the catalog.
import {
  account_tokens_perm_archeion_account_read,
  account_tokens_perm_archeion_tagma_read,
  account_tokens_perm_archeion_tagma_write,
  account_tokens_perm_files_delivery_write,
  account_tokens_perm_files_record_read,
  account_tokens_perm_files_record_write,
} from "../paraglide/messages.js";

export const PERM_IDS = [
  "archeion:tagma:read",
  "archeion:tagma:write",
  "archeion:account:read",
  "files:record:read",
  "files:record:write",
  "files:delivery:write",
] as const;

export type PermId = (typeof PERM_IDS)[number];

/** The strings a tagma credential can charge: it calls the file service but
 *  reaches no registry route, so these three are the whole picker and the set
 *  the migration writes. `perm.rs` holds the same list as `TAGMA_DEFAULTS`;
 *  the test pins the two together. */
export const TAGMA_PERM_IDS: readonly PermId[] = PERM_IDS.filter((p) =>
  p.startsWith("files:"),
);

/** What a new personal token starts with: reads only. Writes are an explicit
 *  choice; the server refuses a token that carries no permission at all. */
export const PAT_DEFAULT_PERMS: readonly PermId[] = [
  "archeion:tagma:read",
  "archeion:account:read",
  "files:record:read",
];

const LABELS: Record<PermId, () => string> = {
  "archeion:tagma:read": account_tokens_perm_archeion_tagma_read,
  "archeion:tagma:write": account_tokens_perm_archeion_tagma_write,
  "archeion:account:read": account_tokens_perm_archeion_account_read,
  "files:record:read": account_tokens_perm_files_record_read,
  "files:record:write": account_tokens_perm_files_record_write,
  "files:delivery:write": account_tokens_perm_files_delivery_write,
};

/** A human label for one permission string. An id outside the vocabulary (a
 *  server that grew a string this build does not know) renders as the raw id
 *  rather than disappearing. */
export function permLabel(id: string): string {
  const label = LABELS[id as PermId];
  return label ? label() : id;
}

/** Toggle one known id inside a stored list that may also hold strings this
 *  build does not know, keeping the vocabulary's order so a saved list reads
 *  the same way every time; an unknown string keeps its place. */
export function toggleStored(current: readonly string[], id: PermId): string[] {
  const without = current.filter((p) => p !== id);
  if (current.includes(id)) return without;
  const rank = (p: string) => {
    const i = (PERM_IDS as readonly string[]).indexOf(p);
    return i === -1 ? PERM_IDS.length : i;
  };
  const at = without.findIndex((p) => rank(p) > rank(id));
  const next = [...without];
  next.splice(at === -1 ? next.length : at, 0, id);
  return next;
}

/** Split a record's permission list into the ids this build knows and the rest,
 *  so the picker can render the known ones as checkboxes and still show an
 *  unknown string instead of silently dropping it. */
export function splitPerms(perms: readonly string[]): {
  known: PermId[];
  unknown: string[];
} {
  const known = PERM_IDS.filter((p) => perms.includes(p));
  const unknown = perms.filter(
    (p) => !(PERM_IDS as readonly string[]).includes(p),
  );
  return { known, unknown };
}
