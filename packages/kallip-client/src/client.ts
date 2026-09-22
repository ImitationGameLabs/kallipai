import {
  KallipError,
  parseSseStream,
  readApiError,
  TransportError,
} from "@kallipai/kallip-common";
import type { AgentId, FileAttachment } from "@kallipai/kallip-common";
import type {
  AgentStatusResponse,
  BudgetResponse,
  BudgetUpdateRequest,
  DeleteSetResponse,
  DirectMessageRow,
  ExternalHistoryResponse,
  LescheSessionEntry,
  ListAgentsManagementResponse,
  ListAgentsQuery,
  MessageResponse,
  ProfileApplyResponse,
  ProfileConfig,
  ProfileConfigPutRequest,
  ProfileProbeRequest,
  ProfileProbeResponse,
  PutWorkScheduleRequest,
  TaskCloseRequest,
  TaskConfirmRequest,
  TaskCreateRequest,
  TaskExport,
  TaskForceRequest,
  TaskListPage,
  TaskListQuery,
  TaskNoteRequest,
  UpdateAgentMetadataRequest,
  UpdateDutyRequest,
  WireAgentSummary,
  WorkSchedule,
} from "./types.ts";

export interface TagmaClientOptions {
  readonly baseUrl: string;
  readonly authToken?: string;
}

/**
 * Low-level HTTP client for the kallip tagma. TypeScript counterpart to the
 * Rust `kallip-client` crate's `TagmaClient`. Browser-first: uses `fetch` with
 * no Node globals. Throws {@link KallipError} on non-2xx (parsed from the
 * `{"error":{"message":...}}` envelope) and {@link TransportError} on network
 * failures.
 */
export class TagmaClient {
  private readonly base: string;
  private readonly token?: string;

  constructor(opts: TagmaClientOptions) {
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
      throw new TransportError(`tagma request failed: ${path}`, { cause });
    }
    if (!resp.ok) {
      throw new KallipError(await readApiError(resp));
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
  private bytes(path: string): Promise<Uint8Array> {
    return this.request(path)
      .then((r) => r.arrayBuffer())
      .then((b) => new Uint8Array(b));
  }

  // --- agent surface ---

  postMessage(
    id: AgentId,
    text: string,
    attachment?: FileAttachment,
  ): Promise<MessageResponse> {
    return this.json<MessageResponse>(`/agents/${id}/message`, {
      method: "POST",
      body: attachment
        ? JSON.stringify({ text, attachment })
        : JSON.stringify({ text }),
    });
  }

  /** Fetch the tagma's single root agent (always present after tagma startup). */
  getRootAgent(): Promise<WireAgentSummary> {
    return this.json<WireAgentSummary>("/agents/root");
  }

  // --- streaming events ---

  /** Subscribe to the agent's external event stream (the chat-room API): one
   * multiplexed SSE discriminated by the `event:` field ("authored" | "signal"
   * | "status"). Yields raw `{ event, data }` frames; the caller decodes each
   * payload per its event name. This is the frontend's sole window onto the
   * tagma -- authored assistant messages, runtime signals, and status
   * snapshots all arrive here. */
  async *externalEventStream(
    id: AgentId,
    signal?: AbortSignal,
    /** Fired when the connection opens and on EVERY raw frame, including
     * keepalive comment frames (they carry no event name and are skipped
     * below). Lets a caller run a liveness watchdog on top of the stream.
     * Optional; omitted by callers that do not need it. */
    onFrame?: () => void,
  ): AsyncGenerator<{ readonly event: string; readonly data: string }> {
    const resp = await this.request(`/agents/${id}/external/events`, {
      method: "GET",
      signal,
    });
    const contentType = resp.headers.get("content-type") ?? "";
    if (!contentType.includes("text/event-stream")) {
      throw new TransportError(
        `expected text/event-stream, got ${contentType}`,
      );
    }
    onFrame?.(); // connection open: initial liveness for the watchdog
    for await (const raw of parseSseStream(resp, signal)) {
      onFrame?.();
      // Keepalive / comment frames carry no `event:` name; skip them. Every
      // real frame on this stream is discriminated by its event name.
      if (!raw.event) continue;
      yield { event: raw.event, data: raw.data };
    }
  }

  /** Pull a cursor-driven history window for the direct (offline) path. The
   * direct SSE is live-only, so the frontend asks for back-log here using its
   * `maxRendered` high-water mark — symmetric with the relay's
   * `TagmaControl::History`. Omit `after`/`before` for the most recent `limit`
   * (a first-time device with an empty cache). */
  externalHistory(
    id: AgentId,
    opts: { after?: number | null; before?: number | null; limit?: number },
  ): Promise<ExternalHistoryResponse> {
    const params = new URLSearchParams();
    if (opts.after != null) params.set("after", String(opts.after));
    if (opts.before != null) params.set("before", String(opts.before));
    if (opts.limit != null) params.set("limit", String(opts.limit));
    const qs = params.toString();
    const path = `/agents/${id}/external/history${qs ? `?${qs}` : ""}`;
    return this.json<ExternalHistoryResponse>(path);
  }

  // --- lesche session surfaces (the root agent's relay conversation data) ---

  /** GET /agents/{id}/lesche/sessions — every relay surface the agent can
   * address (`bilateral` / `room` / `direct`), aggregated best-effort across
   * the online relays. */
  lescheSessions(id: AgentId): Promise<LescheSessionEntry[]> {
    return this.json(`/agents/${id}/lesche/sessions`);
  }

  /** GET /agents/{id}/lesche/direct-sessions/{peer}/messages?format=json —
   * the direct session with `peer` as typed rows (the console UI's
   * transcript contract). `after` is exclusive (rows ascend by `seq`);
   * `limit` defaults server-side to a conservative page sized for the
   * manage bridge's response cap. */
  directSessionHistory(
    id: AgentId,
    peer: string,
    opts: { after?: number | null; limit?: number } = {},
  ): Promise<DirectMessageRow[]> {
    const params = new URLSearchParams({ format: "json" });
    if (opts.after != null) params.set("after_seq", String(opts.after));
    if (opts.limit != null) params.set("limit", String(opts.limit));
    return this.json(
      `/agents/${id}/lesche/direct-sessions/${peer}/messages?${params}`,
    );
  }

  // --- management: budget ---

  /** GET /budget — tagma-wide token budget status. */
  getBudget(): Promise<BudgetResponse> {
    return this.json<BudgetResponse>("/budget");
  }

  /** POST /budget — adjust or set remaining budget (operator-only). */
  updateBudget(body: BudgetUpdateRequest): Promise<BudgetResponse> {
    return this.json<BudgetResponse>("/budget", {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  // --- management: agents ---

  /** GET /agents — list all agents. */
  listAgents(query?: ListAgentsQuery): Promise<ListAgentsManagementResponse> {
    const params = new URLSearchParams();
    if (query?.created_by) params.set("created_by", query.created_by);
    const qs = params.toString();
    return this.json<ListAgentsManagementResponse>(
      `/agents${qs ? `?${qs}` : ""}`,
    );
  }

  /** GET /agents/{id}/status — agent context usage, retries, tagma-wide budget. */
  getAgentStatus(id: string): Promise<AgentStatusResponse> {
    return this.json<AgentStatusResponse>(`/agents/${id}/status`);
  }

  /** POST /agents/{id}/interrupt — cancel the agent's current round. */
  interruptAgent(id: string): Promise<void> {
    return this.request(`/agents/${id}/interrupt`, { method: "POST" }).then(
      () => undefined,
    );
  }

  /** DELETE /agents/{id} — remove an agent (must be idle, no subagents). */
  removeAgent(id: string): Promise<void> {
    return this.request(`/agents/${id}`, { method: "DELETE" }).then(
      () => undefined,
    );
  }

  /** PUT /agents/{id}/duty — set on-duty/off-duty (operator-only). */
  setAgentDuty(id: string, body: UpdateDutyRequest): Promise<void> {
    return this.json(`/agents/${id}/duty`, {
      method: "PUT",
      body: JSON.stringify(body),
    }).then(() => undefined);
  }

  /** PUT /agents/{id}/metadata — update role and/or description. */
  updateAgentMetadata(
    id: string,
    body: UpdateAgentMetadataRequest,
  ): Promise<void> {
    return this.json(`/agents/${id}/metadata`, {
      method: "PUT",
      body: JSON.stringify(body),
    }).then(() => undefined);
  }

  // --- management: profiles ---

  /** GET /profiles — current profile config (operator-only). */
  getProfiles(): Promise<ProfileConfig> {
    return this.json<ProfileConfig>("/profiles");
  }

  /** PUT /profiles — validate, persist, and hot-swap (operator-only). */
  updateProfiles(body: ProfileConfigPutRequest): Promise<ProfileConfig> {
    return this.json<ProfileConfig>("/profiles", {
      method: "PUT",
      body: JSON.stringify(body),
    });
  }

  /** POST /profiles/apply — push current registry to all live agents (operator-only). */
  applyProfiles(): Promise<ProfileApplyResponse> {
    return this.json<ProfileApplyResponse>("/profiles/apply", {
      method: "POST",
    });
  }

  /** POST /profiles/probe — dry-run validation of a (draft) config (operator-only). */
  probeProfiles(body: ProfileProbeRequest): Promise<ProfileProbeResponse> {
    return this.json<ProfileProbeResponse>("/profiles/probe", {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** DELETE /profiles/sets/{name} — remove a set. Bound agents are a
   * conflict unless `force` interrupts them first (the response lists who);
   * the default set and the root's set are refused. */
  deleteProfileSet(name: string, force: boolean): Promise<DeleteSetResponse> {
    return this.json<DeleteSetResponse>(
      `/profiles/sets/${encodeURIComponent(name)}?force=${force}`,
      { method: "DELETE" },
    );
  }

  // --- management: work schedules ---

  /** GET /work-schedule — the tagma's schedule; the migration seeds the
   * row, so it always exists (operator-only). */
  getWorkSchedule(): Promise<WorkSchedule> {
    return this.json<WorkSchedule>("/work-schedule");
  }

  /** PUT /work-schedule — create or replace the tagma schedule (operator-only). */
  putWorkSchedule(body: PutWorkScheduleRequest): Promise<WorkSchedule> {
    return this.json<WorkSchedule>("/work-schedule", {
      method: "PUT",
      body: JSON.stringify(body),
    });
  }

  // --- management: task ledger ---

  /** GET /tasks — one page of lightweight rows plus the filter total. */
  listTasks(query?: TaskListQuery): Promise<TaskListPage> {
    const params = new URLSearchParams();
    if (query?.status) params.set("status", query.status);
    if (query?.assignee) params.set("assignee", query.assignee);
    if (query?.archived !== undefined) {
      params.set("archived", String(query.archived));
    }
    if (query?.time) params.set("time", query.time);
    if (query?.since !== undefined) params.set("since", String(query.since));
    if (query?.until !== undefined) params.set("until", String(query.until));
    if (query?.limit !== undefined) params.set("limit", String(query.limit));
    if (query?.offset !== undefined) params.set("offset", String(query.offset));
    const qs = params.toString();
    return this.json<TaskListPage>(`/tasks${qs ? `?${qs}` : ""}`);
  }

  /** GET /tasks/{id} — one task with its event trail. */
  getTask(id: number): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}`);
  }
  /** POST /tasks — register a task (the creator is the authenticated identity). */
  createTask(body: TaskCreateRequest): Promise<TaskExport> {
    return this.json<TaskExport>("/tasks", {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/start — claim the task; --force escapes the serial gate. */
  startTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/start`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/confirm — file the actor's confirmation, optionally with a report. */
  confirmTask(id: number, body: TaskConfirmRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/confirm`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/review — move an in-progress task to review. */
  reviewTask(id: number): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/review`, { method: "POST" });
  }

  /** POST /tasks/{id}/pause — park an in-progress task. */
  pauseTask(id: number): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/pause`, { method: "POST" });
  }

  /** POST /tasks/{id}/resume — unpause; --force escapes the serial gate. */
  resumeTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/resume`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/note — append a work note to the trail. */
  noteTask(id: number, body: TaskNoteRequest): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/note`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/close — close with a reason; the confirmation gate applies. */
  closeTask(id: number, body: TaskCloseRequest): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/close`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/reopen — reopen a closed task; --force escapes the serial gate. */
  reopenTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/reopen`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** POST /tasks/{id}/archive — archive a closed task; --force overrides the gate. */
  archiveTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/archive`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  /** GET /tasks/{id}/archive — download the closed dossier snapshot. */
  fetchTaskArchive(id: number): Promise<Uint8Array> {
    return this.bytes(`/tasks/${id}/archive`);
  }
  /** GET /tasks/{id}/export — one task plus its trail as JSON. */
  exportTask(id: number): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/export`);
  }

  /** GET /tasks/export — every task plus its trail as JSON. */
  exportAllTasks(): Promise<TaskExport[]> {
    return this.json<TaskExport[]>("/tasks/export");
  }
}
