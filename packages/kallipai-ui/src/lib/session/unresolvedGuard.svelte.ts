// The dead-end guard for /chat/{id}: a conversation whose channel never opens
// is reported as unknown, but that verdict is only meaningful for the id it was
// reached for. A page-level boolean would outlive the navigation that produced
// it and paint the banner over the next conversation; storing the judged id
// keeps the two in step.

// One record rides on it: the id the miss was decided for. Nothing is
// written while an opening is still in flight -- undecided and settled
// look the same -- and the page's effect reads the opening set, so it
// runs again once that set drains and reaches a verdict then.

export interface UnresolvedGuard {
  /** True only once the miss was decided, for this very conversation id. */
  unknown(conversationId: string): boolean;
  /** Record that no channel and no owning tagma could be found. */
  markUnknown(conversationId: string): void;
  /** Drop the record: resolved, owned, or being judged again mid-open. */
  settle(): void;
}

export function createUnresolvedGuard(): UnresolvedGuard {
  let judged = $state<string | undefined>(undefined);
  return {
    unknown: (conversationId) => judged === conversationId,
    markUnknown: (conversationId) => {
      judged = conversationId;
    },
    settle: () => {
      judged = undefined;
    },
  };
}
