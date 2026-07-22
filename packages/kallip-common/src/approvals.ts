import type { AgentId } from "./ids.ts";

// Approval domain types. Field names are camelCase here; the direct client's
// wire adapter renames the tagma's snake_case fields (requested_by, tool_name,
// commit_reason, deny_reason, created_at). The ApprovalStatus *values* stay
// snake_case because the tagma serializes them snake_case even inside the
// camelCase `approvalUpdated` SSE event, so they pass through unchanged.

export type ApprovalStatus =
  | "pending"
  | "committed"
  | "approved"
  | "denied"
  | "redeemed"
  | "cancelled";

export interface ToolCallContent {
  readonly toolName: string;
  // Arbitrary JSON; opaque to the client.
  readonly arguments: unknown;
}

export interface ApprovalEntry {
  readonly id: string;
  readonly requestedBy: AgentId;
  readonly content: ToolCallContent;
  readonly commitReason: string | null;
  readonly status: ApprovalStatus;
  readonly denyReason: string | null;
  // RFC3339 timestamp.
  readonly createdAt: string;
}

export interface ListApprovalsResponse {
  readonly items: ApprovalEntry[];
  readonly total: number;
}

export interface ListApprovalsParams {
  readonly status?: ApprovalStatus;
  readonly limit?: number;
  readonly offset?: number;
  readonly requestedBy?: AgentId;
  readonly order?: "asc" | "desc";
}
