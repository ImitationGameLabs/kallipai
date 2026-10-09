// Tooltip text for a parked agent's row: a compact technical rendering of
// the structured reason (hover-only diagnostics, like the row's raw id --
// not worth localizing the five variants). Lives in a plain module, not in
// statusCard.svelte.ts, so tests can import it without the runes module
// (drafts.ts set the precedent for that split).

import type { WireParkedReason } from "@kallipai/kallipai-client";

export function parkedReasonText(r: WireParkedReason): string {
  // Unit variants arrive as bare strings (serde's externally tagged
  // default); `in` demands an object, so narrow the string arm first.
  if (typeof r === "string") {
    return r === "maxRoundsExceeded"
      ? "max rounds exceeded"
      : "transient retries exhausted";
  }
  if ("failoverChainExhausted" in r) {
    const v = r.failoverChainExhausted;
    return `failover chain exhausted (${v.reason}): ${v.detail}`;
  }
  if ("fatalError" in r) return `fatal error: ${r.fatalError.message}`;
  if ("tokenBudgetExceeded" in r) {
    return `token budget exceeded (${r.tokenBudgetExceeded.consumed}/${r.tokenBudgetExceeded.budget})`;
  }
  return "transient retries exhausted";
}
