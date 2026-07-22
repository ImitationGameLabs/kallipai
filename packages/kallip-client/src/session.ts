import { DIRECT_CAPABILITIES } from "@kallipai/kallip-common";
import type {
  AgentId,
  AgentStatus,
  ApprovalEntry,
  DomainEvent,
  ListApprovalsParams,
  ListApprovalsResponse,
  Session,
  SessionCapabilities,
  TokenBudget,
} from "@kallipai/kallip-common";
import type { TagmaClient } from "./client.ts";

/**
 * A {@link Session} bound to a single tagma agent. Wraps a {@link TagmaClient}
 * and the agent id, exposing the live event stream and the direct-only
 * management surface (interrupt, approvals, status, budget). `close()` aborts
 * the event stream.
 */
export class TagmaSession implements Session {
  readonly capabilities: SessionCapabilities = DIRECT_CAPABILITIES;
  private readonly controller = new AbortController();
  private eventsIterator?: AsyncGenerator<DomainEvent>;

  constructor(
    private readonly client: TagmaClient,
    readonly agentId: AgentId,
  ) {}

  get events(): AsyncGenerator<DomainEvent> {
    // Lazily create the stream once; iterating again resumes the same generator.
    if (!this.eventsIterator) {
      this.eventsIterator = this.client.eventStream(
        this.agentId,
        this.controller.signal,
      );
    }
    return this.eventsIterator;
  }

  async send(text: string): Promise<void> {
    await this.client.postMessage(this.agentId, text);
  }

  async interrupt(): Promise<void> {
    await this.client.interruptAgent(this.agentId);
  }

  close(): Promise<void> {
    this.controller.abort();
    return Promise.resolve();
  }

  // --- direct-only management surface ---

  listApprovals(params?: ListApprovalsParams): Promise<ListApprovalsResponse> {
    return this.client.listApprovals(params);
  }

  getApproval(id: string): Promise<ApprovalEntry> {
    return this.client.getApproval(id);
  }

  respondApproval(
    id: string,
    decision: "approve" | "deny",
    reason?: string,
  ): Promise<void> {
    return this.client.respondApproval(id, decision, reason);
  }

  getAgentStatus(): Promise<AgentStatus> {
    return this.client.agentStatus(this.agentId);
  }

  getTokenBudget(): Promise<TokenBudget> {
    return this.client.getTokenBudget();
  }

  adjustTokenBudget(delta: number): Promise<TokenBudget> {
    return this.client.adjustTokenBudget(delta);
  }

  setTokenBudget(value: number): Promise<TokenBudget> {
    return this.client.setTokenBudget(value);
  }
}
