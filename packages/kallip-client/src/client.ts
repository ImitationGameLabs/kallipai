import {
  KallipError,
  TransportError,
  parseSseStream,
} from "@kallipai/kallip-common";
import type {
  AgentId,
  AgentStatus,
  ApprovalEntry,
  DomainEvent,
  ListApprovalsParams,
  ListApprovalsResponse,
  TokenBudget,
} from "@kallipai/kallip-common";
import type {
  ApprovalDecisionBody,
  CreateAgentRequest,
  CreateAgentResponse,
  MessageResponse,
  RawSseEvent,
  TokenBudgetResponse,
  TokenBudgetUpdateRequest,
  WireAgentStatusResponse,
  WireApprovalEntry,
  WireListAgentsResponse,
  WireListApprovalsResponse,
} from "./types.ts";
import {
  sseToDomain,
  wireApprovalToCommon,
  wireStatusToCommon,
} from "./types.ts";

export interface DaemonClientOptions {
  readonly baseUrl: string;
  readonly authToken?: string;
}

/**
 * Low-level HTTP client for the kallip daemon. TypeScript counterpart to the
 * Rust `kallip-client` crate's `DaemonClient`. Browser-first: uses `fetch` with
 * no Node globals. Throws {@link KallipError} on non-2xx (parsed from the
 * `{"error":{"message":...}}` envelope) and {@link TransportError} on network
 * failures.
 */
export class DaemonClient {
  private readonly base: string;
  private readonly token?: string;

  constructor(opts: DaemonClientOptions) {
    this.base = opts.baseUrl.replace(/\/+$/, "");
    this.token = opts.authToken;
  }

  private headers(extra?: Record<string, string>): Record<string, string> {
    const h: Record<string, string> = { ...extra };
    if (this.token) h["Authorization"] = `Bearer ${this.token}`;
    return h;
  }

  private async request(
    path: string,
    init: RequestInit = {},
  ): Promise<Response> {
    let resp: Response;
    try {
      resp = await fetch(this.base + path, {
        ...init,
        headers: this.headers(
          init.headers as Record<string, string> | undefined,
        ),
      });
    } catch (cause) {
      throw new TransportError(`daemon request failed: ${path}`, { cause });
    }
    if (!resp.ok) {
      throw new KallipError({
        status: resp.status,
        message: await readErrorMessage(resp),
      });
    }
    return resp;
  }

  private json<T>(path: string, init: RequestInit = {}): Promise<T> {
    return this.request(path, {
      ...init,
      headers: {
        "content-type": "application/json",
        ...(init.headers as Record<string, string> | undefined),
      },
    }).then((r) => r.json() as Promise<T>);
  }

  // --- agent lifecycle ---

  spawn(req: CreateAgentRequest): Promise<AgentId> {
    return this.json<CreateAgentResponse>("/agents", {
      method: "POST",
      body: JSON.stringify(req),
    }).then((r) => r.id);
  }

  postMessage(id: AgentId, text: string): Promise<MessageResponse> {
    return this.json<MessageResponse>(`/agents/${id}/message`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
  }

  listAgents(createdBy?: AgentId): Promise<WireListAgentsResponse["agents"]> {
    const qs = createdBy ? `?created_by=${encodeURIComponent(createdBy)}` : "";
    return this.json<WireListAgentsResponse>(`/agents${qs}`).then(
      (r) => r.agents,
    );
  }

  interruptAgent(id: AgentId): Promise<void> {
    return this.request(`/agents/${id}/interrupt`, { method: "POST" }).then(
      () => undefined,
    );
  }

  removeAgent(id: AgentId): Promise<void> {
    return this.request(`/agents/${id}`, { method: "DELETE" }).then(
      () => undefined,
    );
  }

  // --- streaming events ---

  /** Subscribe to the agent's SSE stream, yielding DomainEvents until close. */
  async *eventStream(
    id: AgentId,
    signal?: AbortSignal,
  ): AsyncGenerator<DomainEvent> {
    const resp = await this.request(`/agents/${id}/events`, {
      method: "GET",
      signal,
    });
    const contentType = resp.headers.get("content-type") ?? "";
    if (!contentType.includes("text/event-stream")) {
      throw new TransportError(
        `expected text/event-stream, got ${contentType}`,
      );
    }
    for await (const raw of parseSseStream(resp, signal)) {
      let parsed: RawSseEvent;
      try {
        parsed = JSON.parse(raw.data) as RawSseEvent;
      } catch (cause) {
        throw new TransportError("invalid SSE payload", { cause });
      }
      yield sseToDomain(parsed);
    }
  }

  // --- approvals ---

  async listApprovals(
    params?: ListApprovalsParams,
  ): Promise<ListApprovalsResponse> {
    const r = await this.json<WireListApprovalsResponse>(
      `/approvals${buildApprovalQuery(params)}`,
    );
    return { items: r.items.map(wireApprovalToCommon), total: r.total };
  }

  async getApproval(id: string): Promise<ApprovalEntry> {
    const w = await this.json<WireApprovalEntry>(`/approvals/${id}`);
    return wireApprovalToCommon(w);
  }

  respondApproval(
    id: string,
    decision: "approve" | "deny",
    reason?: string,
  ): Promise<void> {
    const body: ApprovalDecisionBody = {
      decision,
      ...(reason ? { reason } : {}),
    };
    return this.request(`/approvals/${id}`, {
      method: "POST",
      body: JSON.stringify(body),
    }).then(() => undefined);
  }

  // --- status / budget ---

  agentStatus(id: AgentId): Promise<AgentStatus> {
    return this.json<WireAgentStatusResponse>(`/agents/${id}/status`).then(
      wireStatusToCommon,
    );
  }

  getTokenBudget(): Promise<TokenBudget> {
    return this.json<TokenBudgetResponse>("/budget");
  }

  adjustTokenBudget(delta: number): Promise<TokenBudget> {
    const body: TokenBudgetUpdateRequest = { delta };
    return this.json<TokenBudgetResponse>("/budget", {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  setTokenBudget(value: number): Promise<TokenBudget> {
    const body: TokenBudgetUpdateRequest = { set_remaining: value };
    return this.json<TokenBudgetResponse>("/budget", {
      method: "POST",
      body: JSON.stringify(body),
    });
  }
}

async function readErrorMessage(resp: Response): Promise<string> {
  try {
    const body = (await resp.json()) as { error?: { message?: string } };
    const message = body?.error?.message;
    if (message) return message;
  } catch {
    try {
      const text = await resp.text();
      if (text) return text;
    } catch {
      // fall through to statusText
    }
  }
  return resp.statusText;
}

function buildApprovalQuery(params?: ListApprovalsParams): string {
  if (!params) return "";
  const q = new URLSearchParams();
  if (params.status) q.set("status", params.status);
  if (params.limit != null) q.set("limit", String(params.limit));
  if (params.offset != null) q.set("offset", String(params.offset));
  if (params.requestedBy) q.set("requested_by", params.requestedBy);
  if (params.order) q.set("order", params.order);
  const s = q.toString();
  return s ? `?${s}` : "";
}
