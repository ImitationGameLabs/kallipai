// The shared fact table as the fleet panel renders it: one line per fact, so
// an operator sees what this tagma learned about upstream endpoints and
// accounts. Pure and i18n-backed (the same-layer precedent as
// retryFormat.ts), so a test can pin the rendering without a runes module.
// The table is tagma-wide -- the wire sends it once per response, not once per
// agent -- so callers render these lines in exactly one tagma-level place.

import type { FactKind, FactView } from "@kallipai/kallipai-client";
import { formatRemaining } from "../tagmata.svelte.ts";
import {
  tagma_facts_account,
  tagma_facts_kind_endpoint_auth,
  tagma_facts_kind_endpoint_shape,
  tagma_facts_kind_payment,
  tagma_facts_kind_quota,
  tagma_facts_reopens,
  tagma_facts_scope_account,
} from "../../paraglide/messages.js";

/** One rendered fact. `key` is the line's identity across refreshes (the wire
 * carries no fact id), built from the same fields the tagma keys its own
 * endpoint/account entries by; `reopens` is absent when the upstream gave no
 * reset hint, or gave one already past. `endpoint` is the base URL the fact
 * was recorded against, absent on account-wide facts. */
export interface FactLine {
  readonly key: string;
  readonly kind: string;
  readonly scope: string;
  readonly endpoint?: string;
  readonly account: string;
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
    const scope = fact.endpoint?.family ?? tagma_facts_scope_account();
    const until = fact.until;
    const reopens =
      until !== undefined && until > now
        ? tagma_facts_reopens({ when: formatRemaining((until - now) * 1_000) })
        : undefined;
    return {
      // The endpoint's own identity rides in the key: one account and family
      // can carry facts for two bases recorded in the same second, and the
      // keyed each throws on equal keys.
      key: [
        fact.kind,
        fact.endpoint?.family ?? "",
        fact.endpoint?.base_url ?? "",
        fact.account_hint,
        fact.recorded_at,
      ].join(":"),
      kind: KIND_LABELS[fact.kind]?.() ?? fact.kind,
      scope,
      endpoint: fact.endpoint?.base_url,
      account: tagma_facts_account({ hint: fact.account_hint }),
      reopens,
      detail: fact.detail,
    };
  });
}
