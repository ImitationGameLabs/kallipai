// Availability state machine for the signup username field: fold the
// archeion probe's status into a render state, gating on the local shape
// check so an invalid handle never hits the network. Pure logic only --
// the component owns the debounce timer and the probe call, and the
// archeion (`GET /v1/lesche/auth/username-availability`) stays the authority.

import { isValidUsername } from "./username.ts";

/** Pause between the last keystroke and the probe. Long enough that a
 * typing burst fires one request, short enough to feel live. */
export const USERNAME_AVAILABILITY_DEBOUNCE_MS = 3000;

/** The probe statuses the UI can render. `invalid` is folded to idle here:
 * a handle that passed `shouldProbe` cannot come back invalid, and an
 * indicator-less field beats inventing a fourth render state. */
export type AvailabilityProbeStatus = "available" | "taken" | "reserved";

/** `checking` covers the debounce wait AND the in-flight request -- both
 * render as "no verdict yet". */
export type AvailabilityState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "done"; status: AvailabilityProbeStatus };

/** Should the normalized username arm a probe? The local shape check is
 * the gate: it already renders its own hint for invalid shapes, so those
 * never cost a network round-trip. */
export function shouldProbe(normalized: string): boolean {
  return isValidUsername(normalized);
}

/** Fold a server status into the render state. */
export function foldStatus(
  status: "available" | "taken" | "reserved" | "invalid",
): AvailabilityState {
  if (status === "invalid") return { phase: "idle" };
  return { phase: "done", status };
}

/** True when a response carries an outdated probe token: the user kept
 * typing while it was in flight, so the verdict must be discarded. */
export function isStale(token: number, latestToken: number): boolean {
  return token !== latestToken;
}
