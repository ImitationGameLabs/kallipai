// Session re-verification for the realtime SSE loop: pure decision logic,
// split out of realtime.svelte.ts so the 401 recovery policy is testable
// without instantiating the $state-backed store (svelte module transforms are
// not available under the bare Deno test runner). The store owns the verifier
// binding (a shell-injected callback, same sink style as the envelope and
// presence sinks); this module owns the throttle and the verdict mapping.

/** Re-validate the login session against the auth plane. Returns true when
 * the session survives (the 401 was lesche-side), false when the auth plane
 * says the session is really gone. May reject on network trouble. */
export type SessionVerifier = () => Promise<boolean>;

/** How often a lesche 401 may trigger an auth-plane round-trip. Bounds the
 * pathological mismatch (lesche rejects, auth plane is fine) to a 30s retry
 * period instead of a verify-per-reconnect storm. */
export const SESSION_REVERIFY_THROTTLE_MS = 30_000;

/** `valid`: the session survives -- keep the SSE loop alive under backoff.
 * `invalid`: the session is really gone -- stop and clear (the uid-keyed
 * effect in the shell restarts the store on the next sign-in).
 * `skipped`: inside the throttle window -- no round-trip was made; treat as
 * valid and let the backoff, not the verifier, pace the retries. */
export type VerifyVerdict = "valid" | "invalid" | "skipped";

/** Decide what a lesche 401 means. `verifier === null` (shell never bound
 * one) maps to `invalid`: without a way to check, the historical stop-on-401
 * behavior is the safe default. A verifier that rejects (network trouble
 * reaching the auth plane) maps to `valid`: the auth plane being unreachable
 * is transient, not a rejection -- the bounded backoff retries instead of
 * tearing down a session that may still be fine. */
export async function reverifySession(
  verifier: SessionVerifier | null,
  lastVerifiedAt: number,
  now: number,
): Promise<{ verdict: VerifyVerdict; verifiedAt: number }> {
  if (verifier === null) {
    return { verdict: "invalid", verifiedAt: lastVerifiedAt };
  }
  if (now - lastVerifiedAt < SESSION_REVERIFY_THROTTLE_MS) {
    return { verdict: "skipped", verifiedAt: lastVerifiedAt };
  }
  try {
    const ok = await verifier();
    return { verdict: ok ? "valid" : "invalid", verifiedAt: now };
  } catch {
    return { verdict: "valid", verifiedAt: now };
  }
}
