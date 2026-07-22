import type {
  AgentState,
  AgentStatus,
  ApprovalEntry,
  ApprovalStatus,
  DomainEvent,
  FailoverChainExhaustion,
  TokenBudget,
} from "@kallipai/kallip-common";

// ---------------------------------------------------------------------------
// Raw tagma SSE event.
//
// The variant *tags* are camelCase (the enum has `rename_all = "camelCase"`),
// matching DomainEvent. The struct-variant *fields* are NOT renamed (serde's
// enum-level rename_all does not apply to fields without rename_all_fields), so
// `max_attempts` and `delay_secs` stay snake_case on the wire. sseToDomain
// renames exactly those two.
// ---------------------------------------------------------------------------

export type RawSseEvent =
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
      readonly max_attempts: number;
      readonly error: string;
      readonly delay_secs: number;
    }
  | {
      readonly type: "streamReset";
      readonly error: string;
      readonly attempt: number;
      readonly max_attempts: number;
      readonly delay_secs: number;
    }
  | {
      readonly type: "failover";
      readonly from: string;
      readonly to: string;
      readonly reason: string;
    }
  | {
      readonly type: "failoverChainExhausted";
      readonly reason: string;
      readonly detail: string;
    };

export function sseToDomain(ev: RawSseEvent): DomainEvent {
  switch (ev.type) {
    case "reasoning":
      return { type: "reasoning", content: ev.content };
    case "reasoningDelta":
      return { type: "reasoningDelta", delta: ev.delta };
    case "assistantContent":
      return { type: "assistantContent", content: ev.content };
    case "assistantContentDelta":
      return { type: "assistantContentDelta", delta: ev.delta };
    case "toolCall":
      return { type: "toolCall", name: ev.name, args: ev.args };
    case "toolResult":
      return { type: "toolResult", result: ev.result };
    case "finished":
      return { type: "finished", content: ev.content };
    case "busy":
      return { type: "busy" };
    case "status":
      return { type: "status", message: ev.message };
    case "error":
      return { type: "error", message: ev.message };
    case "maxRoundsExceeded":
      return { type: "maxRoundsExceeded" };
    case "interrupted":
      return { type: "interrupted" };
    case "cancelled":
      return { type: "cancelled" };
    case "tokenBudgetExceeded":
      return {
        type: "tokenBudgetExceeded",
        consumed: ev.consumed,
        budget: ev.budget,
      };
    case "approvalUpdated":
      return { type: "approvalUpdated", id: ev.id, status: ev.status };
    case "retrying":
      return {
        type: "retrying",
        attempt: ev.attempt,
        maxAttempts: ev.max_attempts,
        error: ev.error,
        delaySecs: ev.delay_secs,
      };
    case "streamReset":
      return {
        type: "streamReset",
        error: ev.error,
        attempt: ev.attempt,
        maxAttempts: ev.max_attempts,
        delaySecs: ev.delay_secs,
      };
    case "failover":
      return { type: "failover", from: ev.from, to: ev.to, reason: ev.reason };
    case "failoverChainExhausted":
      return {
        type: "failoverChainExhausted",
        reason: ev.reason as FailoverChainExhaustion,
        detail: ev.detail,
      };
  }
}

// ---------------------------------------------------------------------------
// Wire approval types (snake_case) and the adapter to common camelCase.
// ---------------------------------------------------------------------------

export interface WireToolCallContent {
  readonly tool_name: string;
  readonly arguments: unknown;
}

export interface WireApprovalEntry {
  readonly id: string;
  readonly requested_by: string;
  readonly content: WireToolCallContent;
  readonly commit_reason: string | null;
  readonly status: ApprovalStatus;
  readonly deny_reason: string | null;
  readonly created_at: string;
}

export interface WireListApprovalsResponse {
  readonly items: WireApprovalEntry[];
  readonly total: number;
}

export function wireApprovalToCommon(w: WireApprovalEntry): ApprovalEntry {
  return {
    id: w.id,
    requestedBy: w.requested_by,
    content: { toolName: w.content.tool_name, arguments: w.content.arguments },
    commitReason: w.commit_reason,
    status: w.status,
    denyReason: w.deny_reason,
    createdAt: w.created_at,
  };
}

// ---------------------------------------------------------------------------
// Wire agent status (snake_case) -> common AgentStatus. Only the fields the UI
// needs are surfaced; the tagma's full ContextUsage / recent_retries are
// dropped here and can be widened later.
// ---------------------------------------------------------------------------

export interface WireAgentStatusResponse {
  readonly state: AgentState;
  readonly activity: string;
  readonly token_budget: number;
  readonly token_consumed: number;
}

export function wireStatusToCommon(w: WireAgentStatusResponse): AgentStatus {
  return {
    state: w.state,
    activity: w.activity,
    tokenBudget: w.token_budget,
    tokenConsumed: w.token_consumed,
  };
}

export interface WireAgentSummary {
  readonly id: string;
  readonly workspace_root?: string;
  readonly state: AgentState;
  readonly created_by?: string;
  readonly role: string;
  readonly description?: string;
  readonly activity?: string;
  readonly faulted_reason?: string | null;
}

export interface WireListAgentsResponse {
  readonly agents: WireAgentSummary[];
}

// ---------------------------------------------------------------------------
// Request / response bodies.
// ---------------------------------------------------------------------------

export type MaxToolRoundsWire = "unlimited" | { readonly limited: number };

export interface CreateAgentRequest {
  readonly workspace_root?: string;
  readonly skills?: string[];
  readonly prompt?: string;
  readonly created_by?: string;
  readonly role: string;
  readonly description?: string;
  readonly max_tool_rounds?: MaxToolRoundsWire;
  readonly permission_class?: string;
}

export interface CreateAgentResponse {
  readonly id: string;
}

export interface MessageResponse {
  readonly queue_depth: number;
  readonly warning?: string;
}

export interface ApprovalDecisionBody {
  readonly decision: "approve" | "deny";
  readonly reason?: string;
}

export interface TokenBudgetUpdateRequest {
  readonly set_remaining?: number;
  readonly delta?: number;
}

// TokenBudgetResponse has the same field names as common TokenBudget
// (budget / consumed / remaining), so it maps directly.
export type TokenBudgetResponse = TokenBudget;
