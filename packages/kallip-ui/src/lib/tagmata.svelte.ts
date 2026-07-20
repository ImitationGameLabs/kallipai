// Tagmata-dashboard view-models and projections. Pure shapes + helpers -- no
// transport, no `@kallipai/kallip-agora-client` import -- so `kallip-ui` stays
// prop-driven and portable. The consuming app (kallip-web) maps agora-client
// response types into these `Props` before passing them down.

/** Props for one enrolled-tagma card (`GET /v1/tagmata` row). */
export interface TagmaCardProps {
  readonly tagmaId: string;
  readonly label: string | null;
  /** RFC3339. */
  readonly createdAt: string;
  /** True iff a herald tunnel is live right now (the sole liveness signal). */
  readonly online: boolean;
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

/** Per-section load state for the dashboard (drives auto-hide + skeleton/error). */
export type SectionPhase = "loading" | "loaded" | "error";

/** Skeleton background token for the online/offline dot (mirrors connection VM). */
export function onlineDotClass(online: boolean): string {
  return online ? "bg-success-500" : "bg-surface-400";
}

/** Locale-formatted timestamp for an RFC3339 string. */
export function formatDateTime(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
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
