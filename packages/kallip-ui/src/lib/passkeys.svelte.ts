// Passkey-manager view-models. Pure shapes + helpers -- no transport, no
// `@kallipai/kallip-agora-client` import -- so the prop-driven components stay
// portable. The consuming page maps agora-client response types into these
// `Props` before passing them down (mirrors `tagmata.svelte.ts`).

/** Load phase of the passkeys section. */
export type PasskeyPhase = "loading" | "loaded" | "error";

/** Props for one passkey row (`GET /v1/me/passkeys` entry). The label may be
 *  empty (the initial passkey is unnamed until the user renames it); the card
 *  renders a fallback. `lastUsedAt` is seeded to the enrollment instant. */
export interface PasskeyCardProps {
  readonly id: string;
  readonly label: string;
  readonly createdAt: string;
  readonly lastUsedAt: string;
}

/** A human-facing summary of an add-device ceremony outcome, ready to render. */
export interface PasskeyAddHint {
  readonly tone: "ok" | "err";
  readonly text: string;
}

/** A freshly minted device-pairing code, shown once on the authenticated device
 *  (as a typeable code AND a QR encoding the same code) with a live countdown.
 *  `expiresAt` is RFC3339. */
export interface PairingCodeView {
  readonly code: string;
  readonly expiresAt: string;
}

/** Seconds remaining until `expiresAt` (RFC3339), clamped at 0. */
export function pairSecondsRemaining(expiresAt: string, now: number): number {
  const ms = Date.parse(expiresAt) - now;
  return Math.max(0, Math.ceil(ms / 1000));
}

/** Format a passkey `created_at` (RFC3339) as a compact date. Date-only
 *  granularity is deliberate (the time of day a credential was added is not
 *  useful to a user managing their devices). */
export function formatPasskeyDate(ts: string): string {
  return new Date(ts).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
