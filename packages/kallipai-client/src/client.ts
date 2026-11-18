import { Effect } from "effect";
import {
  fetchRequest,
  JSON_REQUEST_TIMEOUT_MS,
  jsonRequest,
  KallipaiError,
  parseSseStream,
  readApiError,
  runClientEffect,
  sseRequest,
  TransportError,
  UNREADABLE_BODY_MESSAGE,
} from "@kallipai/kallipai-common";
import type {
  AgentId,
  FileAttachment,
  RequestFailure,
} from "@kallipai/kallipai-common";
import type {
  AgentStatusResponse,
  BodyRange,
  BudgetResponse,
  BudgetUpdateRequest,
  CallBody,
  CallPart,
  DeleteSetResponse,
  ExternalHistoryResponse,
  ListAgentsManagementResponse,
  ListAgentsQuery,
  MessageBody,
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
  TextPart,
  TurnDetail,
  TurnQuery,
  TurnsPage,
  UpdateAgentMetadataRequest,
  UpdateDutyRequest,
  WireAgentSummary,
  WorkSchedule,
} from "./types.ts";

export interface TagmaClientOptions {
  readonly baseUrl: string;
  readonly authToken?: string;
}

/** The query string a body read sends; empty when nothing was asked. */
function bodyQuery(query?: BodyRange): string {
  const params = new URLSearchParams();
  if (query?.part) params.set("part", query.part);
  if (query?.offset !== undefined) params.set("offset", String(query.offset));
  if (query?.limit !== undefined) params.set("limit", String(query.limit));
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

/** The windows the profile verbs take instead of the seam's 30s. They are the
 * only routes here whose answer waits on something the tagma does not control,
 * and they wait in different shapes.
 *
 * A pull is two serial exchanges of the gateway (`profile_source/proxy.rs`
 * `fetch_and_adapt_inner`: `/selected-collection`, then `/parking`), each with
 * a 60s whole-request window, so a healthy but slow pull can take about 120s.
 * A probe spends up to 20s on the provider calls (`PROBE_TIMEOUT`, once for
 * the model list and once for the balance when the family has none) and then
 * up to 60s on the real inference (`INFERENCE_TIMEOUT`), shared by every
 * profile naming the same (provider, model); the phases run in sequence, so
 * the worst case is about 80s. A 30s window would cut either of them
 * while still healthy.
 */
const PROFILE_PROBE_TIMEOUT_MS = 90_000;
const PROFILE_PULL_TIMEOUT_MS = 150_000;

/** The seam's plane-neutral failure as the classes this client throws: the
 * split callers already branch on stays put. An answer that arrived is a
 * `KallipaiError` carrying its envelope (a 409's dangling list included); no
 * answer at all is a `TransportError`. */
function requestFailure(
  failure: RequestFailure,
): KallipaiError | TransportError {
  if (failure.kind === "transport") {
    return new TransportError(failure.message, { cause: failure.cause });
  }
  return new KallipaiError({
    status: failure.status,
    message: failure.message,
    ...(failure.code !== undefined ? { code: failure.code } : {}),
    ...(failure.dangling !== undefined ? { dangling: failure.dangling } : {}),
  });
}

/**
 * Low-level HTTP client for the kallipai tagma. TypeScript counterpart to the
 * Rust `kallipai-client` crate's `TagmaClient`. Browser-first: uses `fetch` with
 * no Node globals. Throws {@link KallipaiError} on non-2xx (parsed from the
 * `{"error":{"message":...}}` envelope) and {@link TransportError} on network
 * failures.
 *
 * The transport is the shared seam: `json`, `status` and `bytes` cross
 * `runClientEffect` once, `raw` stays an Effect, and the event stream
 * opens with `sseRequest` and parses with `parseSseStream`.
 */
export class TagmaClient {
  private readonly base: string;
  private readonly token?: string;

  constructor(opts: TagmaClientOptions) {
    this.base = opts.baseUrl.replace(/\/+$/, "");
    this.token = opts.authToken;
  }

  /** The direct path authenticates with a bearer the caller supplied; the
   * tagma reads no cookie on these routes, so the credential mode below is
   * this plane's declaration and these are the only headers it adds. */
  private authHeaders(): Record<string, string> {
    return this.token ? { Authorization: `Bearer ${this.token}` } : {};
  }

  /** One JSON exchange through the shared seam: this plane's bearer and
   * credential mode, the seam's window, body encoding and envelope parse. */
  private json<T>(
    path: string,
    options: {
      readonly method?: string;
      readonly body?: unknown;
      /** Overrides the shared window for a route the tagma itself may
       * spend longer on; `null` sends no window. */
      readonly timeoutMs?: number | null;
    } = {},
  ): Promise<T> {
    return runClientEffect(
      Effect.mapError(
        jsonRequest<T>({
          baseUrl: this.base,
          path,
          method: options.method ?? "GET",
          body: options.body,
          headers: this.authHeaders(),
          credentials: "omit",
          timeoutMs: options.timeoutMs,
        }),
        requestFailure,
      ),
    );
  }

  /** One raw exchange: the response itself, for the download that must not be
   * decoded here and for the verbs that read the status line alone. The
   * refusal mapping below is the one `externalEventStream` repeats; keep the
   * two in step. */
  private raw(
    path: string,
    options: { readonly method?: string; readonly timeoutMs: number | null },
  ): Effect.Effect<Response, TransportError | KallipaiError> {
    const headers = this.authHeaders();
    return Effect.flatMap(
      fetchRequest({
        baseUrl: this.base,
        path,
        method: options.method ?? "GET",
        headers,
        credentials: "omit",
        timeoutMs: options.timeoutMs,
      }),
      (resp) =>
        resp.ok
          ? Effect.succeed(resp)
          : Effect.flatMap(
              Effect.promise(() => readApiError(resp)),
              (api) => Effect.fail(new KallipaiError(api)),
            ),
    );
  }

  /** The status line is all these verbs read: a success is a success whatever
   * the body says, and a refusal is the usual envelope error. */
  private status(path: string, method: string): Promise<void> {
    return runClientEffect(
      Effect.asVoid(
        this.raw(path, { method, timeoutMs: JSON_REQUEST_TIMEOUT_MS }),
      ),
    );
  }

  private bytes(path: string): Promise<Uint8Array> {
    return runClientEffect(
      Effect.map(
        Effect.flatMap(this.raw(path, { timeoutMs: null }), (resp) =>
          Effect.tryPromise({
            try: () => resp.arrayBuffer(),
            catch: (cause) =>
              new TransportError(UNREADABLE_BODY_MESSAGE, { cause }),
          }),
        ),
        (buffer) => new Uint8Array(buffer),
      ),
    );
  }

  // --- agent surface ---

  postMessage(
    id: AgentId,
    text: string,
    attachment?: FileAttachment,
    /** The 1:1 history row id this message quotes, when the sender
     *  answered one. Absent leaves the historical body shape untouched. */
    quote?: number,
  ): Promise<MessageResponse> {
    // A send carrying neither optional piece keeps the historical
    // `{"text":...}` bytes: `JSON.stringify` drops undefined fields.
    return this.json<MessageResponse>(`/agents/${id}/message`, {
      method: "POST",
      body: { text, attachment, quote },
    });
  }

  /** Fetch the tagma's single root agent (always present after tagma startup). */
  getRootAgent(): Promise<WireAgentSummary> {
    return this.json<WireAgentSummary>("/agents/root");
  }

  // --- streaming events ---

  /** Subscribe to the agent's external event stream (the chat-room API): one
   * multiplexed SSE discriminated by the `event:` field ("authored" | "signal"
   * | "status" | "missed" | "closing"). Yields raw `{ event, data }` frames; the
   * caller decodes each payload per its event name. `missed` carries no payload:
   * it says frames were lost and the reader repairs from its own cursor. `closing`
   * says the tagma is shutting down and nothing follows. This is the frontend's
   * sole window onto the tagma -- new messages, runtime signals, status snapshots,
   * the repair marker and the closing frame all arrive here.
   * The ways it ends other than by frames: a refusal is a `KallipaiError`; a
   * dead exchange or a wrong content type is a `TransportError`; a caller that
   * aborts before the headers rejects with its `signal.reason`; once the body
   * runs, an abort ends the loop cleanly and an error inside it surfaces as it
   * is. */
  async *externalEventStream(
    id: AgentId,
    signal?: AbortSignal,
    /** Fired when the connection opens and after every non-empty chunk of
     * the body arrives -- including keepalive comment blocks, which carry
     * no event name and are skipped below. Liveness is bytes on the wire,
     * not parsed events, so a watchdog over this cannot starve on a quiet
     * but healthy stream. Optional; omitted by callers that do not need
     * it. */
    onLiveness?: () => void,
  ): AsyncGenerator<{ readonly event: string; readonly data: string }> {
    // The connect window is the seam's: it bounds how long the stream may take
    // to produce headers, and nothing after the headers is cut by it.
    const resp = await runClientEffect(
      sseRequest(`${this.base}/agents/${id}/external/events`, {
        credentials: "omit",
        headers: this.authHeaders(),
      }),
      signal,
    );
    if (!resp.ok) {
      // The seam hands the status line back unread on purpose; this plane's
      // refusal is the same envelope error every verb throws.
      throw new KallipaiError(await readApiError(resp));
    }
    const contentType = resp.headers.get("content-type") ?? "";
    if (!contentType.includes("text/event-stream")) {
      throw new TransportError(
        `expected text/event-stream, got ${contentType}`,
      );
    }
    onLiveness?.(); // connection open: initial liveness for the watchdog
    for await (const raw of parseSseStream(resp, signal, onLiveness)) {
      // Keepalive / comment frames carry no `event:` name; skip them. Every
      // real frame on this stream is discriminated by its event name.
      if (!raw.event) continue;
      yield { event: raw.event, data: raw.data };
    }
  }

  /** Pull a history page for the direct (offline) path. The direct SSE is
   * live-only and lossy, so the frontend asks for the log here: `afterSeq`
   * continues from the last position it rendered, `beforeSeq` reads the page
   * above a scroll-up anchor, `row` asks for one row by id (the read behind a
   * quoted reference), and omitting all three reads the newest `limit` rows (a
   * first-time device with an empty cache). `next` in the answer says where
   * the same request continues. Symmetric with the relay's
   * `TagmaControl::History`, in position numbering rather than row ids. */
  externalHistory(
    id: AgentId,
    opts: {
      afterSeq?: number | null;
      beforeSeq?: number | null;
      /** One row by id; never combined with a position cursor. */
      row?: number | null;
      limit?: number;
    },
  ): Promise<ExternalHistoryResponse> {
    const params = new URLSearchParams();
    if (opts.afterSeq != null) params.set("after_seq", String(opts.afterSeq));
    if (opts.beforeSeq != null)
      params.set("before_seq", String(opts.beforeSeq));
    if (opts.row != null) params.set("row", String(opts.row));
    if (opts.limit != null) params.set("limit", String(opts.limit));
    const qs = params.toString();
    const path = `/agents/${id}/external/history${qs ? `?${qs}` : ""}`;
    return this.json<ExternalHistoryResponse>(path);
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
      body,
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
    return this.status(`/agents/${id}/interrupt`, "POST");
  }

  /** DELETE /agents/{id} — remove an agent (must be idle, no subagents). */
  removeAgent(id: string): Promise<void> {
    return this.status(`/agents/${id}`, "DELETE");
  }

  /** PUT /agents/{id}/duty — set on-duty/off-duty (operator-only). */
  setAgentDuty(id: string, body: UpdateDutyRequest): Promise<void> {
    return this.json(`/agents/${id}/duty`, {
      method: "PUT",
      body,
    }).then(() => undefined);
  }

  /** PUT /agents/{id}/metadata — update role and/or description. */
  updateAgentMetadata(
    id: string,
    body: UpdateAgentMetadataRequest,
  ): Promise<void> {
    return this.json(`/agents/${id}/metadata`, {
      method: "PUT",
      body,
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
      timeoutMs: PROFILE_PULL_TIMEOUT_MS,
      method: "PUT",
      body,
    });
  }

  /** POST /profiles/apply — push current registry to all live agents (operator-only). */
  applyProfiles(): Promise<ProfileApplyResponse> {
    return this.json<ProfileApplyResponse>("/profiles/apply", {
      method: "POST",
    });
  }

  /** POST /profiles/refresh — re-pull the live gateway snapshot and swap
   * the bundle (operator-only; a local source is a 409). */
  refreshProfiles(): Promise<ProfileConfig> {
    return this.json<ProfileConfig>("/profiles/refresh", {
      timeoutMs: PROFILE_PULL_TIMEOUT_MS,
      method: "POST",
    });
  }

  /** POST /profiles/probe — dry-run validation of a (draft) config (operator-only). */
  probeProfiles(body: ProfileProbeRequest): Promise<ProfileProbeResponse> {
    return this.json<ProfileProbeResponse>("/profiles/probe", {
      timeoutMs: PROFILE_PROBE_TIMEOUT_MS,
      method: "POST",
      body,
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
      body,
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
      body,
    });
  }

  /** POST /tasks/{id}/start — claim the task; --force escapes the serial gate. */
  startTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/start`, {
      method: "POST",
      body,
    });
  }

  /** POST /tasks/{id}/confirm — file the actor's confirmation, optionally with a report. */
  confirmTask(id: number, body: TaskConfirmRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/confirm`, {
      method: "POST",
      body,
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
      body,
    });
  }

  /** POST /tasks/{id}/note — append a work note to the trail. */
  noteTask(id: number, body: TaskNoteRequest): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/note`, {
      method: "POST",
      body,
    });
  }

  /** POST /tasks/{id}/close — close with a reason; the confirmation gate applies. */
  closeTask(id: number, body: TaskCloseRequest): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/close`, {
      method: "POST",
      body,
    });
  }

  /** POST /tasks/{id}/reopen — reopen a closed task; --force escapes the serial gate. */
  reopenTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/reopen`, {
      method: "POST",
      body,
    });
  }

  /** POST /tasks/{id}/archive — archive a closed task; --force overrides the gate. */
  archiveTask(id: number, body: TaskForceRequest = {}): Promise<TaskExport> {
    return this.json<TaskExport>(`/tasks/${id}/archive`, {
      method: "POST",
      body,
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

  // --- settings surface ---

  /** GET /settings/timezone — the configured IANA timezone name, or null
   * when unset. Any authenticated principal may read. */
  getTimezone(): Promise<{ timezone: string | null }> {
    return this.json<{ timezone: string | null }>("/settings/timezone");
  }

  /** PUT /settings/timezone — set (operator-only) or clear (null) the
   * display timezone. The server validates IANA names and answers 400 on
   * a miss; the thrown KallipaiError carries that message. */
  putTimezone(timezone: string | null): Promise<{ timezone: string | null }> {
    return this.json<{ timezone: string | null }>("/settings/timezone", {
      method: "PUT",
      body: { timezone },
    });
  }

  // --- management: turn history ---

  /** GET /agents/{id}/history -- one page of an agent's turn log, newest
   * first. `before` is the previous page's `next_cursor` (older); absent
   * when the log is exhausted. */
  history(id: string, query?: TurnQuery): Promise<TurnsPage> {
    const params = new URLSearchParams();
    if (query?.day) params.set("day", query.day);
    if (query?.before) params.set("before", query.before);
    if (query?.limit !== undefined) params.set("limit", String(query.limit));
    const qs = params.toString();
    return this.json<TurnsPage>(
      `/agents/${encodeURIComponent(id)}/history${qs ? `?${qs}` : ""}`,
    );
  }

  /** GET /agents/{id}/history/{turn} -- one turn, its messages carrying the
   * shortened text and reasoning text a page leaves out. */
  turn(id: string, turnId: number): Promise<TurnDetail> {
    return this.json<TurnDetail>(
      `/agents/${encodeURIComponent(id)}/history/${turnId}`,
    );
  }

  /** GET /agents/{id}/history/{turn}/calls/{index}/body -- one byte range of
   * a call's stored text. `part` picks the result record (the default), the
   * arguments or the error; omit `offset` for the first range and follow
   * `next_offset` for the next one. */
  callBody(
    id: string,
    turnId: number,
    callIndex: number,
    query?: BodyRange<CallPart>,
  ): Promise<CallBody> {
    const qs = bodyQuery(query);
    return this.json<CallBody>(
      `/agents/${encodeURIComponent(id)}/history/${turnId}/calls/${callIndex}/body${qs}`,
    );
  }

  /** GET /agents/{id}/history/{turn}/messages/{index}/body -- one byte
   * range of one message's own text, or of the reasoning text stored beside
   * it. Same paging as a call body. */
  messageBody(
    id: string,
    turnId: number,
    messageIndex: number,
    query?: BodyRange<TextPart>,
  ): Promise<MessageBody> {
    const qs = bodyQuery(query);
    return this.json<MessageBody>(
      `/agents/${encodeURIComponent(id)}/history/${turnId}/messages/${messageIndex}/body${qs}`,
    );
  }
}
