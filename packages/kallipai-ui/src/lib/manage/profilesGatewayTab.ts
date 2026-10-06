import type { ProfileSourceSelection } from "@kallipai/kallipai-client";

import type {
  UserCollectionRow,
  UserPlatformCollectionRow,
} from "../gateway/client.ts";

/** A pull-selection row: own collections and catalog rows share one
 * key space for the pick. */
export type PullRow = UserCollectionRow | UserPlatformCollectionRow;

/** The row key of the pull selection: own collections by name, catalog
 * rows by owner/name (collection names forbid "/", so the two
 * families never collide). One definition serves the page (the
 * baseline pick) and the preview (the radio state). */
export function collectionRowKey(row: PullRow): string {
  return "owner" in row ? `${row.owner}/${row.name}` : row.name;
}

/** The inputs of the gateway tab's posture: the source health block's
 * stable facts plus the shell context (the online shell runs on one
 * platform's domain, and that platform is the switch target). */
export interface GatewayTabPostureInput {
  sourceMode: string;
  proxyAvailable: boolean;
  online: boolean;
  sessionReady: boolean;
  servingPolis: string | null;
  platforms?: readonly { origin: string; enrolled: boolean }[] | null;
  connectedPolis: string | null;
}

/** The derived posture of the profiles page's gateway tab. */
export interface GatewayTabPosture {
  /** The tab exists for the connected mirror or the online switch
   * entry; the offline direct shell keeps the local-only page. */
  showGatewayTab: boolean;
  /** The serving platform carries an enrollment: the switch target is
   * actionable here. */
  servingPlatformEnrolled: boolean;
  /** The collection browse renders (a user session is the only
   * prerequisite) with an actionable target. */
  browseFace: boolean;
  /** Connected to this page's platform: the narrowing edits act on
   * the live face. */
  activeHere: boolean;
  /** The pick renders wherever an action can consume the selection:
   * the connected-here save, or the pre-switch choice while
   * local on an enrolled platform. */
  selectionEditable: boolean;
}

/** Derive the gateway tab's posture from the source block and the
 * shell context. Pure: the page feeds its live inputs and tests walk
 * the state space. */
export function gatewayTabPosture(
  input: GatewayTabPostureInput,
): GatewayTabPosture {
  const readOnly = input.sourceMode === "model-gateway";
  const servingPlatformEnrolled =
    input.servingPolis !== null &&
    (input.platforms?.some(
      (p) => p.origin === input.servingPolis && p.enrolled,
    ) ??
      false);
  const showGatewayTab = readOnly || (input.proxyAvailable && input.online);
  const activeHere =
    readOnly &&
    input.servingPolis !== null &&
    input.connectedPolis === input.servingPolis;
  return {
    showGatewayTab,
    servingPlatformEnrolled,
    browseFace:
      showGatewayTab &&
      input.sessionReady &&
      (readOnly || servingPlatformEnrolled),
    activeHere,
    selectionEditable: activeHere || (!readOnly && servingPlatformEnrolled),
  };
}

/** The live baseline under the connected source: the selected row
 * covers the live sets (the gateway filters some members away, so
 * the live sets are what remains of the selected row: a row missing
 * a live set cannot be the source, and among covering rows the one
 * carrying the fewest filtered-away members wins; ties keep row
 * order). Null with no rows, an empty live snapshot, or no covering
 * row: nothing is derived. */
export function derivedPullKey(
  rows: readonly PullRow[],
  liveSetNames: Iterable<string>,
): string | null {
  if (rows.length === 0) return null;
  const live = new Set(liveSetNames);
  if (live.size === 0) return null;
  let best: string | null = null;
  let bestExtras = Number.POSITIVE_INFINITY;
  for (const row of rows) {
    let covers = true;
    for (const s of live) {
      if (!row.sets.includes(s)) {
        covers = false;
        break;
      }
    }
    if (!covers) continue;
    let extras = 0;
    for (const s of row.sets) {
      if (!live.has(s)) extras += 1;
    }
    if (extras < bestExtras) {
      best = collectionRowKey(row);
      bestExtras = extras;
    }
  }
  return best;
}

/** The wire member for a picked row key: own rows address the
 * account's space (no owner); catalog rows carry their owner. Null
 * when the key names no row (the rows reloaded and the key went
 * stale; the caller then sends nothing and keeps the persisted
 * selection). */
export function selectionWire(
  rows: readonly PullRow[],
  key: string,
): ProfileSourceSelection | null {
  for (const row of rows) {
    if (collectionRowKey(row) !== key) continue;
    return "owner" in row
      ? { owner: row.owner, collection: row.name }
      : { collection: row.name };
  }
  return null;
}
/** The touched-pick dirty flag: a touched pick that departs from the live
 * baseline. An empty live snapshot (no selection yet) is itself a
 * baseline: any touched pick departs from it, so the first choice
 * is saveable. */
export function pullSelectionDirty(
  activeHere: boolean,
  selection: string | null,
  derivedKey: string | null,
): boolean {
  return activeHere && selection !== null && selection !== derivedKey;
}

/** The save button's enablement: the two pending changes are
 * mode-exclusive — the gateway mirror is read-only (no form
 * edits exist to save), and the pick dirties only under the
 * connected gateway source (a local page has no pick to
 * commit). The diagonal reads: read-only saves the pick,
 * editable saves the form. */
export function saveButtonEnabled(
  readOnly: boolean,
  formDirty: boolean,
  pickDirty: boolean,
): boolean {
  return readOnly ? pickDirty : formDirty;
}
