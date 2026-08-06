// Tagmata-dashboard view-models and projections. Pure shapes + helpers -- no
// transport, no `@kallipai/kallip-agora-client` import -- so `kallip-ui` stays
// prop-driven and portable. The consuming app (kallip-web) maps agora-client
// response types into these `Props` before passing them down.

/** Liveness of an enrolled tagma, as shown by the dashboard dot. `checking`
 * means presence has not yet resolved for this session (the realtime SSE has
 * not delivered its snapshot); the card shows a neutral placeholder rather
 * than a misleading default "offline". */
export type TagmaPresence = "checking" | "online" | "offline";

/** A tagma agent's lifecycle state. Mirrors the tagma's `AgentState` wire enum
 * (`idle` / `busy` / `faulted`); `faulted` is also reported for the root during
 * the transient zero-root recovery window. */
export type TagmaAgentState = "idle" | "busy" | "faulted";

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
}

/** Props for one enrolled-tagma card (`GET /v1/tagmata` row). */
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
 *  plaintext straight from the mint response (while `copyable`), or the agora's
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
      return "bg-surface-400";
    case "checking":
      return "bg-surface-400 animate-pulse";
  }
}

/** Human-readable presence label for the dot caption + tooltip. */
export function presenceLabel(presence: TagmaPresence): string {
  switch (presence) {
    case "online":
      return "online";
    case "offline":
      return "offline";
    case "checking":
      return "checking…";
  }
}

/** Locale-formatted timestamp for an RFC3339 string. */
export function formatDateTime(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** Compact token-count formatting: `12k`, `1.2M`, or the raw number below 1k.
 * Uses SI suffixes (not locale-aware compact notation) for a stable, compact
 * dashboard readout. */
export function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 100) / 10}k`;
  return String(n);
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
  const agents = `${active}/${total} ${total === 1 ? "agent" : "agents"}`;
  const tokens = `${formatTokenCount(s.tokenConsumed)}/${formatTokenCount(
    s.tokenBudget,
  )} tokens`;
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
  if (ms <= 0) return "expired";
  const days = Math.floor(ms / 86_400_000);
  const hours = Math.floor((ms % 86_400_000) / 3_600_000);
  const minutes = Math.floor((ms % 3_600_000) / 60_000);
  const parts: string[] = [];
  if (days > 0) parts.push(`${days}d`);
  if (hours > 0) parts.push(`${hours}h`);
  if (minutes > 0) parts.push(`${minutes}min`);
  return parts.length === 0 ? "<1min" : parts.join(" ");
}
