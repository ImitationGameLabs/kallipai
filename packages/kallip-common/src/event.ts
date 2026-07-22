import type { ApprovalStatus } from "./approvals.ts";

// The unified event union. One variant per semantically distinct thing that can
// happen, abstracting away which transport produced it. Variant discriminators
// are camelCase, mirroring the tagma SseEvent serde tag (rename_all =
// "camelCase"), so the direct client's sseEventToDomain is close to a cast.
//
// Over agora only a small subset ever fires (finished, busy, error, and a
// synthetic idle/status); the streaming/tool/retry/failover variants are
// direct-only.

export type FailoverChainExhaustion =
  | "noFailoverConfigured"
  | "allBackupsExhausted"
  | "allCandidatesUnbuildable"
  | "allCandidatesInfeasible";

export type DomainEvent =
  | { readonly type: "reasoning"; readonly content: string }
  | { readonly type: "reasoningDelta"; readonly delta: string }
  | { readonly type: "assistantContent"; readonly content: string }
  | { readonly type: "assistantContentDelta"; readonly delta: string }
  | { readonly type: "toolCall"; readonly name: string; readonly args: string }
  | { readonly type: "toolResult"; readonly result: string }
  | { readonly type: "finished"; readonly content: string }
  | { readonly type: "busy" }
  | { readonly type: "status"; readonly message: string }
  | { readonly type: "error"; readonly message: string }
  | { readonly type: "maxRoundsExceeded" }
  | { readonly type: "interrupted" }
  | { readonly type: "cancelled" }
  | {
      readonly type: "tokenBudgetExceeded";
      readonly consumed: number;
      readonly budget: number;
    }
  | {
      readonly type: "approvalUpdated";
      readonly id: string;
      readonly status: ApprovalStatus;
    }
  | {
      readonly type: "retrying";
      readonly attempt: number;
      readonly maxAttempts: number;
      readonly error: string;
      readonly delaySecs: number;
    }
  | {
      readonly type: "streamReset";
      readonly error: string;
      readonly attempt: number;
      readonly maxAttempts: number;
      readonly delaySecs: number;
    }
  | {
      readonly type: "failover";
      readonly from: string;
      readonly to: string;
      readonly reason: string;
    }
  | {
      readonly type: "failoverChainExhausted";
      readonly reason: FailoverChainExhaustion;
      readonly detail: string;
    };

// Turn-boundary events: queued user input is flushed when any of these arrive
// (port of kallip-tui/src/tui/events.rs is_boundary).
const BOUNDARY_TYPES: ReadonlySet<DomainEvent["type"]> = new Set([
  "toolCall",
  "finished",
  "cancelled",
  "interrupted",
  "error",
  "maxRoundsExceeded",
  "failoverChainExhausted",
  "tokenBudgetExceeded",
]);

export function isBoundary(event: DomainEvent): boolean {
  return BOUNDARY_TYPES.has(event.type);
}

// Mirrors the Display impl of the Rust FailoverChainExhaustion enum.
export function failoverChainExhaustionToProse(
  reason: FailoverChainExhaustion,
): string {
  switch (reason) {
    case "noFailoverConfigured":
      return "no failover configured";
    case "allBackupsExhausted":
      return "all backups exhausted";
    case "allCandidatesUnbuildable":
      return "all candidates unbuildable";
    case "allCandidatesInfeasible":
      return "all candidates infeasible";
  }
}
