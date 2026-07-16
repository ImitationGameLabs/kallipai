import type { SessionCapabilities } from "./capabilities.ts";
import type { DomainEvent } from "./event.ts";
import type {
  ApprovalEntry,
  ListApprovalsParams,
  ListApprovalsResponse,
} from "./approvals.ts";

// Lightweight, transport-agnostic views of the daemon's status/budget payloads.
// The direct client's wire adapter maps the full snake_case daemon structs into
// these camelCase shapes.

export type AgentState = "idle" | "busy" | "faulted";

export interface AgentStatus {
  readonly state: AgentState;
  readonly activity: string;
  readonly tokenBudget: number;
  readonly tokenConsumed: number;
}

export interface TokenBudget {
  readonly budget: number;
  readonly consumed: number;
  readonly remaining: number;
}

// The contract both transports implement. The UI holds one Session and consumes
// `events` through the transcript reducer. The management surface is optional:
// it is present on the direct transport and undefined under agora, so the UI
// gates the corresponding controls on the method's (and capability's) presence.
export interface Session {
  readonly capabilities: SessionCapabilities;
  readonly events: AsyncIterable<DomainEvent>;

  /** Send a user prompt / message. */
  send(text: string): Promise<void>;

  /** Interrupt a busy turn (direct only). */
  interrupt?(): Promise<void>;

  /** Tear down the session (closes the event stream). */
  close(): Promise<void>;

  // --- direct-only management surface (undefined under agora) ---

  listApprovals?(params?: ListApprovalsParams): Promise<ListApprovalsResponse>;
  getApproval?(id: string): Promise<ApprovalEntry>;
  respondApproval?(
    id: string,
    decision: "approve" | "deny",
    reason?: string,
  ): Promise<void>;
  getAgentStatus?(): Promise<AgentStatus>;
  getTokenBudget?(): Promise<TokenBudget>;
  adjustTokenBudget?(delta: number): Promise<TokenBudget>;
  setTokenBudget?(value: number): Promise<TokenBudget>;
}
