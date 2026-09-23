// Tagmata-dashboard view-models and projections. Pure shapes + helpers -- no
// transport, no `@kallipai/kallip-archeion-client` import -- so `kallip-ui` stays
// prop-driven and portable. The consuming app (kallip-web) maps archeion-client
// response types into these `Props` before passing them down.

import { formatStampInZone } from "./time/stamp.ts";
import {
  remaining_days,
  remaining_hours,
  remaining_minutes,
  remaining_under_minute,
  shell_status_agents_one,
  shell_status_agents_other,
  shell_status_tokens,
  tagma_presence_checking,
  tagma_presence_offline,
  tagma_presence_online,
  tagma_status_unlimited,
  tagmata_expired_badge,
} from "../paraglide/messages.js";
/** Liveness of an enrolled tagma, as shown by the dashboard dot. `checking`
 * means presence has not yet resolved for this session (the realtime SSE has
 * not delivered its snapshot); the card shows a neutral placeholder rather
 * than a misleading default "offline". */
export type TagmaPresence = "checking" | "online" | "offline";

/** A tagma agent's lifecycle state. Mirrors the tagma's `AgentState` wire enum
 * in full (all six values reach the SSE snapshot; `waiting`/`parked`/
 * `retrying` included); `faulted` is also reported for the root during the
 * transient zero-root recovery window. */
export type TagmaAgentState =
  | "idle"
  | "busy"
  | "waiting"
  | "retrying"
  | "parked"
  | "faulted";

/** A tagma's live runtime snapshot, fed by the `tagma_status` SSE event. The
 * root agent (the conversation peer) is reported separately from subagents
 * (spawned helpers) so the UI can distinguish "root processing the user's
 * turn" from "helpers doing background work". TS-idiomatic camelCase; the wire
 * variant is snake_case and mapped at the realtime dispatch boundary.
 * `undefined` while no snapshot has arrived yet (freshly connected dashboard,
 * or an offline tagma). */
export interface TagmaStatusSummary {
  /** The root agent's lifecycle state. */
  readonly rootState: TagmaAgentState;
  /** Every subagent entry, including faulted. */
  readonly subagentsTotal: number;
  /** Subagents currently in the `busy` state. */
  readonly subagentsActive: number;
  /** Tagma-wide token budget (limit). */
  readonly tokenBudget: number;
  /** Cumulative tokens consumed against the budget. */
  readonly tokenConsumed: number;
  /** Budget is unlimited (enforcement off): render the denominator as
   * the localized "Unlimited" label instead of a number. */
  readonly tokenBudgetUnlimited: boolean;
}

/** Props for one enrolled-tagma card (`GET /v1/lesche/tagmata` row). */
export interface TagmaCardProps {
  readonly tagmaId: string;
  readonly label: string | null;
  /** RFC3339. */
  readonly createdAt: string;
  /** Live presence, driven by the realtime SSE. See {@link TagmaPresence}. */
  readonly presence: TagmaPresence;
  /** Live aggregate status, driven by the `tagma_status` SSE event. Omitted
   * while no snapshot has arrived (e.g. offline tagma); the card hides the
   * status line then. See {@link TagmaStatusSummary}. */
  readonly status?: TagmaStatusSummary;
}

/** Props for one pending-tagma card. `code` is the display value: the full
 *  plaintext straight from the mint response (while `copyable`), or the archeion's
 *  masked `sk-enroll-abc***xyz` from the list endpoint. */
export interface EnrollmentCodeCardProps {
  readonly id: string;
  /** Owner-set label; `null` renders as "Unnamed tagma". */
  readonly label: string | null;
  /** RFC3339. */
  readonly createdAt: string;
  /** RFC3339. */
  readonly expiresAt: string;
  /** Full plaintext (just-minted) or masked display value (refreshed). */
  readonly code: string;
  /** True only while `code` is the just-minted full plaintext (Copy available). */
  readonly copyable: boolean;
}

/** The hosted-process half the panel joins onto identity cards. Structural:
 * the instances `InstanceInfo` satisfies it, so this pure view-model file
 * needs no transport-module import. */
export interface TagmaProcessLike {
  readonly slug: string;
  readonly workspace: string;
  readonly running: boolean;
  /** The daemon-scan-read listen port, when the instance is running. */
  readonly port?: number | null;
  /** The daemon-scan-read enrolled tagma identity, when present. */
  readonly tagma_id?: string | null;
}

/** One row of the tagmata panel: an enrolled identity half, a hosted process
 * half, or both merged onto one card. */
export interface TagmaDeviceRow {
  readonly key: string;
  readonly tagma?: TagmaCardProps;
  readonly process?: {
    readonly slug: string;
    readonly workspace: string;
    readonly running: boolean;
    readonly port?: number;
  };
}

/** Join enrolled identities (archeion `/v1/lesche/tagmata`) with hosted processes
 * (instances list) into panel rows. Key preference: the process-reported
 * `tagma_id` (the daemon scan reads the tagma's own persisted id — manual
 * slugs and enroll-after-spawn join here), falling back to the one-click
 * slug convention via `slugFor` so legacy one-click instances keep merging.
 * Unmatched halves keep their own card; a process claimed once cannot be
 * claimed again (a colliding identity renders identity-only). Pure:
 * presence/status/slug lookups come in as callbacks. */
export function joinDeviceRows(
  enrolled: readonly Omit<TagmaCardProps, "presence">[],
  processes: readonly TagmaProcessLike[],
  spawnedPorts: Readonly<Record<string, number>>,
  slugFor: (tagmaId: string) => string,
  presenceFor: (tagmaId: string) => TagmaPresence,
  statusFor: (tagmaId: string) => TagmaStatusSummary | undefined,
): TagmaDeviceRow[] {
  const byTagmaId = new Map<string, TagmaProcessLike>();
  const bySlug = new Map<string, TagmaProcessLike>();
  for (const inst of processes) {
    bySlug.set(inst.slug, inst);
    const id = inst.tagma_id?.trim();
    if (id) byTagmaId.set(id, inst);
  }
  const claimed = new Set<string>();
  const processOf = (inst: TagmaProcessLike) => ({
    slug: inst.slug,
    workspace: inst.workspace,
    running: inst.running,
    port: inst.port ?? spawnedPorts[inst.slug],
  });
  const rows: TagmaDeviceRow[] = [];
  for (const c of enrolled) {
    const inst = byTagmaId.get(c.tagmaId) ?? bySlug.get(slugFor(c.tagmaId));
    const tagma: TagmaCardProps = {
      ...c,
      presence: presenceFor(c.tagmaId),
      status: statusFor(c.tagmaId),
    };
    if (inst && !claimed.has(inst.slug)) {
      claimed.add(inst.slug);
      rows.push({ key: c.tagmaId, tagma, process: processOf(inst) });
    } else {
      rows.push({ key: c.tagmaId, tagma });
    }
  }
  for (const inst of processes) {
    if (claimed.has(inst.slug)) continue;
    rows.push({ key: inst.slug, process: processOf(inst) });
  }
  return rows;
}

/** Per-section load state for the dashboard (drives auto-hide + skeleton/error).
 * Re-exported from the shared `phase.ts` home so cross-feature dashboards (rooms,
 * ...) import from there, not from this tagma module. */
export type { SectionPhase } from "./phase.ts";

/** Skeleton background token for the presence dot. `checking` is a muted,
 * gently pulsing dot to read as "checking", distinct from a definite offline. */
export function presenceDotClass(presence: TagmaPresence): string {
  switch (presence) {
    case "online":
      return "bg-success-500";
    case "offline":
      return "bg-surface-400-600";
    case "checking":
      return "bg-surface-400-600 animate-pulse";
  }
}

/** Human-readable presence label for the dot caption + tooltip. */
export function presenceLabel(presence: TagmaPresence): string {
  switch (presence) {
    case "online":
      return tagma_presence_online();
    case "offline":
      return tagma_presence_offline();
    case "checking":
      return tagma_presence_checking();
  }
}

/** Locale-formatted timestamp for an RFC3339 string. */
export function formatDateTime(iso: string, timezone?: string | null): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? iso
    : formatStampInZone(iso, undefined, timezone);
}

/** Compact token-count formatting: `12k`, `1.2M`, or the raw number below 1k.
 * Uses SI suffixes (not locale-aware compact notation) for a stable, compact
 * dashboard readout. */
export function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 100) / 10}k`;
  return String(n);
}

/** Context occupancy readout for one agent row: `12k / 50k`, `12k / —`
 * while the registry pull is pending, or `—` when the agent has no live
 * context read (faulted/parked agents). Shared by the detail card and
 * the drawer list, which render the same triple inline otherwise. */
export function contextOccupancy(
  used: number | null,
  max: number | null,
): string {
  if (used === null) return "—";
  if (max === null) return `${formatTokenCount(used)} / —`;
  return `${formatTokenCount(used)} / ${formatTokenCount(max)}`;
}

/** One-line status summary for a tagma card: `2/4 agents · 12k/50k tokens`.
 * Aggregate (root + subagents) — the dashboard card stays a glanceable summary;
 * the channel-chat header is where root/sub are shown separately. The root
 * always counts toward the total (1) even when `rootState === "faulted"`.
 * Caller checks for `undefined` before calling (no status yet -> hidden). */
export function formatTagmaStatusLine(s: TagmaStatusSummary): string {
  // Root counts as 1 slot (present even when faulted); active adds root only
  // when it is busy.
  const total = 1 + s.subagentsTotal;
  const active = (s.rootState === "busy" ? 1 : 0) + s.subagentsActive;
  const agents =
    total === 1
      ? shell_status_agents_one({ count: active, total })
      : shell_status_agents_other({ count: active, total });
  const tokens = shell_status_tokens({
    consumed: formatTokenCount(s.tokenConsumed),
    total: s.tokenBudgetUnlimited
      ? tagma_status_unlimited()
      : formatTokenCount(s.tokenBudget),
  });
  return `${agents} · ${tokens}`;
}

/** Whether an RFC3339 expiry has already passed. */
export function isExpired(iso: string): boolean {
  const d = new Date(iso);
  return !Number.isNaN(d.getTime()) && d.getTime() <= Date.now();
}

/**
 * Format a remaining duration (ms) as a compact countdown, dropping leading
 * zero units: `1d 2h 3min`, `2h 3min`, `3min`, `<1min`. `<= 0` -> `expired`.
 * Pure; callers pass `expiresAt - now` so a reactive `now` drives the countdown.
 */
export function formatRemaining(ms: number): string {
  if (ms <= 0) return tagmata_expired_badge();
  const days = Math.floor(ms / 86_400_000);
  const hours = Math.floor((ms % 86_400_000) / 3_600_000);
  const minutes = Math.floor((ms % 3_600_000) / 60_000);
  const parts: string[] = [];
  if (days > 0) parts.push(remaining_days({ d: days }));
  if (hours > 0) parts.push(remaining_hours({ h: hours }));
  if (minutes > 0) parts.push(remaining_minutes({ m: minutes }));
  return parts.length === 0 ? remaining_under_minute() : parts.join(" ");
}
