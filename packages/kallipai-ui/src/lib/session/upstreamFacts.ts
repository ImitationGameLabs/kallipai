// The shared fact table as the fleet panel renders it: one line per fact, so
// an operator sees what this tagma learned about a profile. Pure and
// i18n-backed (the same-layer precedent as retryFormat.ts), so a test can
// pin the rendering without a runes module.
// The table is tagma-wide -- the wire sends it once per response, not once per
// agent -- so callers render these lines in exactly one tagma-level place.

import type { FactKind, FactView } from "@kallipai/kallipai-client";
import { formatRemaining } from "../tagmata.svelte.ts";
import {
  tagma_facts_kind_endpoint_auth,
  tagma_facts_kind_endpoint_shape,
  tagma_facts_kind_payment,
  tagma_facts_kind_quota,
  tagma_facts_reopens,
} from "../../paraglide/messages.js";

/** One rendered fact. `key` is the line's identity across refreshes (the wire
 * carries no fact id), built from the same fields the tagma keys its own
 * profile records by; `reopens` is absent when the upstream gave no reset
 * hint, or gave one already past. `profile` names the profile the fact is
 * about -- the only subject the table has. */
export interface FactLine {
  readonly key: string;
  readonly kind: string;
  readonly profile: string;
  readonly reopens?: string;
  readonly detail: string;
}

// A kind this bundle predates falls back to its wire token rather than to a
// label it cannot back (the parked-reason precedent).
const KIND_LABELS: Record<FactKind, () => string> = {
  quota: tagma_facts_kind_quota,
  payment: tagma_facts_kind_payment,
  endpoint_auth: tagma_facts_kind_endpoint_auth,
  endpoint_shape: tagma_facts_kind_endpoint_shape,
};

/** One line per fact, in the wire's own order (the tagma sorts newest first).
 * `now` is unix seconds: the countdown reads against it, so a caller that
 * re-renders on a tick keeps the text fresh. */
export function factLines(
  facts: readonly FactView[],
  now: number,
): readonly FactLine[] {
  return facts.map((fact) => {
    const until = fact.until;
    const reopens =
      until !== undefined && until > now
        ? tagma_facts_reopens({ when: formatRemaining((until - now) * 1_000) })
        : undefined;
    return {
      // The table keeps one record per profile, so the profile id is what tells
      // two lines apart; the record time keeps a refreshed fact from reusing
      // its predecessor's key.
      key: [fact.kind, fact.profile, fact.recorded_at].join(),
      kind: KIND_LABELS[fact.kind]?.() ?? fact.kind,
      profile: fact.profile,
      reopens,
      detail: fact.detail,
    };
  });
}
