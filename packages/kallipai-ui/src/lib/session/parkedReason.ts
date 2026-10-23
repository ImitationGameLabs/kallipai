// Tooltip text for a parked agent's row: a compact technical rendering of
// the structured reason (hover-only diagnostics, like the row's raw id --
// not worth localizing the variants). Lives in a plain module, not in
// statusCard.svelte.ts, so tests can import it without the runes module
// (drafts.ts set the precedent for that split). `now` is the unix-second
// clock the relative reset hint reads, so a caller re-rendering per tick
// keeps it fresh and a test can pin it.

import type { WireParkedReason } from "@kallipai/kallipai-client";

/** Compact relative hint for a moment still ahead: `2h 3min`, `3min`,
 * `<1min`, `3d 2h`. Local to this module on purpose: the tooltip is
 * non-localized technical text, so the shared countdown buckets (which carry
 * their own wording) would be the wrong face here. */
function remainingHint(now: number, until: number): string {
  const seconds = until - now;
  if (seconds < 60) return "<1min";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}min`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

/** A variant this bundle predates: claiming a concrete cause would be a claim
 * the bundle cannot back (a newer tagma can add one), so name the gap instead
 * -- with the variant key when the payload carries one, which is what an
 * operator would look up. */
function unrecognized(name?: string): string {
  return name
    ? `parked (unrecognized reason: ${name})`
    : "parked (unrecognized reason)";
}

export function parkedReasonText(r: WireParkedReason, now: number): string {
  // Unit variants arrive as bare strings (serde's externally tagged
  // default); `in` demands an object, so narrow the string arm first.
  if (typeof r === "string") {
    if (r === "maxRoundsExceeded") return "max rounds exceeded";
    if (r === "transientRetryExhausted") return "transient retries exhausted";
    return unrecognized(r);
  }
  if ("failoverChainExhausted" in r) {
    const v = r.failoverChainExhausted;
    return `failover chain exhausted (${v.reason}): ${v.detail}`;
  }
  if ("fatalError" in r) return `fatal error: ${r.fatalError.message}`;
  if ("tokenBudgetExceeded" in r) {
    return `token budget exceeded (${r.tokenBudgetExceeded.consumed}/${r.tokenBudgetExceeded.budget})`;
  }
  if ("quotaBlocked" in r) {
    const v = r.quotaBlocked;
    // The reset clause shows only while the moment is still ahead: a hint
    // that already passed is not a promise, and the fact table's own read
    // side drops the fact by then (this is the clock-skew guard).
    const reset =
      v.reset !== undefined && v.reset > now
        ? `, reopens in ${remainingHint(now, v.reset)}`
        : "";
    return `quota blocked (${v.profile}${reset}): ${v.detail}`;
  }
  return unrecognized(Object.keys(r)[0]);
}
