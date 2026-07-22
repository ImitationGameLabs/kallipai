// Tagmata-dashboard view-models and projections. Pure shapes + helpers -- no
// transport, no `@kallipai/kallip-agora-client` import -- so `kallip-ui` stays
// prop-driven and portable. The consuming app (kallip-web) maps agora-client
// response types into these `Props` before passing them down.

/** Liveness of an enrolled tagma, as shown by the dashboard dot. `checking`
 * means presence has not yet resolved for this session (the realtime SSE has
 * not delivered its snapshot); the card shows a neutral placeholder rather
 * than a misleading default "offline". */
export type TagmaPresence = "checking" | "online" | "offline";

/** Props for one enrolled-tagma card (`GET /v1/tagmata` row). */
export interface TagmaCardProps {
  readonly tagmaId: string;
  readonly label: string | null;
  /** RFC3339. */
  readonly createdAt: string;
  /** Live presence, driven by the realtime SSE. See {@link TagmaPresence}. */
  readonly presence: TagmaPresence;
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
