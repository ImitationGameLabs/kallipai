// ManagementBackend: transport-agnostic interface for tagma management operations.
//
// Two implementations:
//   - OfflineBackend: wraps TagmaClient (HTTP to localhost tagma API)
//   - OnlineBackend: wraps ManageRestClient (plaintext over TLS via lesche proxy)
//
// Both throw KallipError on non-2xx (OnlineBackend reconstructs it from the
// relayed status+body). Network failures differ: OfflineBackend throws
// TransportError; OnlineBackend surfaces fetch's own TypeError.

import type { TagmaClient } from "@kallipai/kallip-client";
import {
  LinearBackoff,
  ManageRestClient,
  ProjectionClient,
} from "@kallipai/kallip-lesche-client";
import { KallipError, parseErrorEnvelope } from "@kallipai/kallip-common";
import type {
  AgentStatusResponse,
  BudgetResponse,
  BudgetUpdateRequest,
  DeleteSetResponse,
  ListAgentsManagementResponse,
  ListAgentsQuery,
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
  WorkSchedule,
} from "@kallipai/kallip-client";

/** A live projection-dirty subscription, when the transport has
 * one (OnlineBackend over the lesche SSE). `subscribe` starts the
 * feed and returns the stop handle; the implementation owns the
 * reconnect/backoff loop and fans every dirty nudge to the callback. */
export interface ProjectionFeed {
  subscribe(onDirty: () => void): () => void;
}

/** The 14 management methods shared by both backends. */
export interface ManagementBackend {
  /** Present only on transports with a live dirty feed. */
  readonly projectionFeed?: ProjectionFeed;

  getBudget(): Promise<BudgetResponse>;
  updateBudget(body: BudgetUpdateRequest): Promise<BudgetResponse>;
  listAgents(query?: ListAgentsQuery): Promise<ListAgentsManagementResponse>;
  getAgentStatus(id: string): Promise<AgentStatusResponse>;
  interruptAgent(id: string): Promise<void>;
  removeAgent(id: string): Promise<void>;
  setAgentDuty(id: string, body: UpdateDutyRequest): Promise<void>;
  updateAgentMetadata(
    id: string,
    body: UpdateAgentMetadataRequest,
  ): Promise<void>;
  getProfiles(): Promise<ProfileConfig>;
  updateProfiles(body: ProfileConfigPutRequest): Promise<ProfileConfig>;
  applyProfiles(): Promise<ProfileApplyResponse>;
  probeProfiles(body: ProfileProbeRequest): Promise<ProfileProbeResponse>;
  deleteProfileSet(name: string, force: boolean): Promise<DeleteSetResponse>;
  getWorkSchedule(): Promise<WorkSchedule>;
  putWorkSchedule(body: PutWorkScheduleRequest): Promise<WorkSchedule>;
  listTasks(query?: TaskListQuery): Promise<TaskListPage>;
  getTask(id: number): Promise<TaskExport>;
  createTask(body: TaskCreateRequest): Promise<TaskExport>;
  startTask(id: number, body?: TaskForceRequest): Promise<TaskExport>;
  confirmTask(id: number, body?: TaskConfirmRequest): Promise<TaskExport>;
  reviewTask(id: number): Promise<TaskExport>;
  pauseTask(id: number): Promise<TaskExport>;
  resumeTask(id: number, body?: TaskForceRequest): Promise<TaskExport>;
  noteTask(id: number, body: TaskNoteRequest): Promise<TaskExport>;
  closeTask(id: number, body: TaskCloseRequest): Promise<TaskExport>;
  reopenTask(id: number, body?: TaskForceRequest): Promise<TaskExport>;
  archiveTask(id: number, body?: TaskForceRequest): Promise<TaskExport>;
  fetchTaskArchive(id: number): Promise<Uint8Array>;
}

// --- OfflineBackend (wraps TagmaClient) ---

export class OfflineBackend implements ManagementBackend {
  constructor(private readonly client: TagmaClient) {}

  getBudget() {
    return this.client.getBudget();
  }
  updateBudget(body: BudgetUpdateRequest) {
    return this.client.updateBudget(body);
  }
  listAgents(query?: ListAgentsQuery) {
    return this.client.listAgents(query);
  }
  getAgentStatus(id: string) {
    return this.client.getAgentStatus(id);
  }
  interruptAgent(id: string) {
    return this.client.interruptAgent(id);
  }
  removeAgent(id: string) {
    return this.client.removeAgent(id);
  }
  setAgentDuty(id: string, body: UpdateDutyRequest) {
    return this.client.setAgentDuty(id, body);
  }
  updateAgentMetadata(id: string, body: UpdateAgentMetadataRequest) {
    return this.client.updateAgentMetadata(id, body);
  }
  getProfiles() {
    return this.client.getProfiles();
  }
  updateProfiles(body: ProfileConfigPutRequest) {
    return this.client.updateProfiles(body);
  }
  applyProfiles() {
    return this.client.applyProfiles();
  }

  probeProfiles(body: ProfileProbeRequest) {
    return this.client.probeProfiles(body);
  }

  deleteProfileSet(name: string, force: boolean) {
    return this.client.deleteProfileSet(name, force);
  }
  getWorkSchedule() {
    return this.client.getWorkSchedule();
  }
  putWorkSchedule(body: PutWorkScheduleRequest) {
    return this.client.putWorkSchedule(body);
  }
  listTasks(query?: TaskListQuery) {
    return this.client.listTasks(query);
  }
  getTask(id: number) {
    return this.client.getTask(id);
  }
  createTask(body: TaskCreateRequest) {
    return this.client.createTask(body);
  }
  startTask(id: number, body?: TaskForceRequest) {
    return this.client.startTask(id, body);
  }
  confirmTask(id: number, body?: TaskConfirmRequest) {
    return this.client.confirmTask(id, body);
  }
  reviewTask(id: number) {
    return this.client.reviewTask(id);
  }
  pauseTask(id: number) {
    return this.client.pauseTask(id);
  }
  resumeTask(id: number, body?: TaskForceRequest) {
    return this.client.resumeTask(id, body);
  }
  noteTask(id: number, body: TaskNoteRequest) {
    return this.client.noteTask(id, body);
  }
  closeTask(id: number, body: TaskCloseRequest) {
    return this.client.closeTask(id, body);
  }
  reopenTask(id: number, body?: TaskForceRequest) {
    return this.client.reopenTask(id, body);
  }
  archiveTask(id: number, body?: TaskForceRequest) {
    return this.client.archiveTask(id, body);
  }
  fetchTaskArchive(id: number) {
    return this.client.fetchTaskArchive(id);
  }
}

// --- OnlineBackend (wraps ManageRestClient over the lesche proxy) ---

/**
 * Reconstruct a KallipError from a manage_result when status >= 400. The
 * body is the tagma's `ApiError` envelope (see parseErrorEnvelope), relayed
 * verbatim — including the structured dangling list a 409 profiles save
 * carries for the confirm flow.
 */
function parseError(status: number, body: unknown): Error {
  switch (typeof body) {
    case "string": {
      // The reverse proxy emits plain-text bodies on its own error
      // paths (403/404/502/504); fold into the same KallipError shape.
      return new KallipError({
        status,
        message: body || "management request failed",
      });
    }
  }
  const { message, dangling } = parseErrorEnvelope(body);
  return new KallipError({
    status,
    message: message ?? "management request failed",
    dangling,
  });
}

export class OnlineBackend implements ManagementBackend {
  readonly projectionFeed?: ProjectionFeed;
  constructor(
    private readonly rest: ManageRestClient,
    private readonly agent: string,
    private readonly projection?: ProjectionClient,
  ) {
    if (!projection) return;
    // A single shared reconnect loop -- one sse connection per
    // backend regardless of subscriber count. Subscribers join/leave a
    // listener set; the first subscribe starts the loop (fresh
    // controller + backoff), the last unsubscribe aborts it, and the
    // next subscribe starts a new one, so any stop handle only ever
    // retires its own callback.
    const listeners = new Set<() => void>();
    const backoff = new LinearBackoff();
    let controller: AbortController | null = null;
    let warnedThisStreak = false;
    // Dirty-frame traffic shaping, shared by every
    // subscriber. A stream storm (one dirty frame per mutation) must
    // not multiply into one GET per frame.
    let lastSeq = 0; // seq gate: a frame at or below the last notified
    // seq is a replay or echo, not news
    let windowTimer: ReturnType<typeof setTimeout> | null = null;
    let windowDirty = false;
    let pendingWhileHidden = false;
    const notifyListeners = (): void => {
      for (const fn of [...listeners]) fn();
    };
    // Leading edge: the first frame in the window notifies immediately;
    // frames landing within the next 750ms fold into the window, and
    // the window tail fires one catch-up nudge if anything folded
    // (750ms sits well under any human perceivable lag yet absorbs a
    // burst of same-tick mutations).
    const onFrame = (seq: number): void => {
      if (seq <= lastSeq) return;
      lastSeq = seq;
      if (document.hidden) {
        pendingWhileHidden = true;
        return;
      }
      if (windowTimer === null) {
        notifyListeners();
        windowTimer = setTimeout(() => {
          windowTimer = null;
          if (windowDirty) {
            windowDirty = false;
            notifyListeners();
          }
        }, 750);
      } else {
        windowDirty = true;
      }
    };
    const onVisibility = (): void => {
      if (!document.hidden && pendingWhileHidden) {
        pendingWhileHidden = false;
        notifyListeners();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    const runLoop = async (): Promise<void> => {
      while (controller && !controller.signal.aborted) {
        try {
          // A fresh connection renumbers the stream: a lesche restart
          // starts seq back at 1, so a gate persisted across connections
          // would drop every frame as a replay. Reset per connection --
          // the cost is one idempotent replayed GET after a reconnect.
          lastSeq = 0;
          for await (const frame of projection.state(
            this.agent,
            controller.signal,
          )) {
            backoff.reset();
            warnedThisStreak = false;
            onFrame(frame.seq);
          }
        } catch (e) {
          // Stream error: fall through to the backoff. Warn once per
          // streak (a flapping endpoint must not spam the console at
          // the reconnect pace); the next clean frame re-arms it.
          if (!warnedThisStreak) {
            console.warn("[projection feed] stream dropped:", e);
            warnedThisStreak = true;
          }
        }
        if (!controller || controller.signal.aborted) return;
        await new Promise((r) => setTimeout(r, backoff.next()));
      }
    };
    this.projectionFeed = {
      subscribe: (onDirty: () => void): (() => void) => {
        listeners.add(onDirty);
        backoff.reset();
        if (!controller) {
          // Revival re-arms the visibility flush: the last unsubscribe
          // detached the document-level listener, so a resurrected feed
          // must re-add it (idempotent per DOM) or hidden frames would
          // pend with no one left to flush them.
          document.addEventListener("visibilitychange", onVisibility);
          controller = new AbortController();
          void runLoop();
        }
        return () => {
          listeners.delete(onDirty);
          if (listeners.size === 0 && controller) {
            controller.abort();
            controller = null;
            // Last one out: detach the visibility listener too, so a
            // retired feed leaves no document-level callbacks behind.
            document.removeEventListener("visibilitychange", onVisibility);
          }
        };
      },
    };
  }
  private async req<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const result = await this.rest.manage(this.agent, method, path, body);
    if (result.status >= 400) throw parseError(result.status, result.body);
    return result.body as T;
  }

  private async reqVoid(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<void> {
    const result = await this.rest.manage(this.agent, method, path, body);
    if (result.status >= 400) throw parseError(result.status, result.body);
  }

  getBudget() {
    return this.req<BudgetResponse>("GET", "/budget");
  }
  updateBudget(body: BudgetUpdateRequest) {
    return this.req<BudgetResponse>("POST", "/budget", body);
  }
  /** The roster read rides the lesche's cached projection
   * instead of the per-call manage relay, so roster refreshes stop
   * round-tripping to the tagma. Falls back to the relay when no
   * projection client was wired (older construction sites). The
   * projection's per-agent summary is shape-compatible with the
   * management one (same serde wire), plus a `lock` field the
   * management type does not declare -- harmless extra at runtime.
   * `created_by` filters only the Offline transport either way.
   */
  listAgents(query?: ListAgentsQuery) {
    if (this.projection) {
      const p = this.projection;
      return p.agents(this.agent).then(
        (r): ListAgentsManagementResponse => ({
          // Same serde wire as the management summary (the projection is a
          // cached copy); the cast only widens optional-detail typing.
          agents: r.agents as ListAgentsManagementResponse["agents"],
        }),
      );
    }
    const qs = query?.created_by
      ? `?created_by=${encodeURIComponent(query.created_by)}`
      : "";
    return this.req<ListAgentsManagementResponse>("GET", `/agents${qs}`);
  }
  getAgentStatus(id: string) {
    return this.req<AgentStatusResponse>(
      "GET",
      `/agents/${encodeURIComponent(id)}/status`,
    );
  }
  interruptAgent(id: string) {
    return this.reqVoid("POST", `/agents/${encodeURIComponent(id)}/interrupt`);
  }
  removeAgent(id: string) {
    return this.reqVoid("DELETE", `/agents/${encodeURIComponent(id)}`);
  }
  setAgentDuty(id: string, body: UpdateDutyRequest) {
    return this.reqVoid("PUT", `/agents/${encodeURIComponent(id)}/duty`, body);
  }
  updateAgentMetadata(id: string, body: UpdateAgentMetadataRequest) {
    return this.reqVoid(
      "PUT",
      `/agents/${encodeURIComponent(id)}/metadata`,
      body,
    );
  }
  getProfiles() {
    return this.req<ProfileConfig>("GET", "/profiles");
  }
  updateProfiles(body: ProfileConfigPutRequest) {
    return this.req<ProfileConfig>("PUT", "/profiles", body);
  }
  applyProfiles() {
    return this.req<ProfileApplyResponse>("POST", "/profiles/apply");
  }

  probeProfiles(body: ProfileProbeRequest) {
    return this.req<ProfileProbeResponse>("POST", "/profiles/probe", body);
  }

  deleteProfileSet(name: string, force: boolean) {
    return this.req<DeleteSetResponse>(
      "DELETE",
      `/profiles/sets/${encodeURIComponent(name)}?force=${force}`,
    );
  }
  getWorkSchedule() {
    return this.req<WorkSchedule>("GET", "/work-schedule");
  }
  putWorkSchedule(body: PutWorkScheduleRequest) {
    return this.req<WorkSchedule>("PUT", "/work-schedule", body);
  }
  listTasks(query?: TaskListQuery) {
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
    return this.req<TaskListPage>("GET", `/tasks${qs ? `?${qs}` : ""}`);
  }
  getTask(id: number) {
    return this.req<TaskExport>("GET", `/tasks/${id}`);
  }
  // The lesche manage-frame allowlist only passes GET /tasks and
  // GET /tasks/{id}: every task write verb and the archive download
  // are offline-transport-only, so the online implementations fail
  // loudly instead of gambling on a proxy 404 (or worse, a JSON-
  // decoded binary for the archive).
  createTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  startTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  confirmTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  reviewTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  pauseTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  resumeTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  noteTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  closeTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  reopenTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  archiveTask(): Promise<TaskExport> {
    return Promise.reject(
      new Error(
        "task writes are offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
  fetchTaskArchive(): Promise<Uint8Array> {
    return Promise.reject(
      new Error(
        "archive download is offline-only: the lesche manage frame allowlist passes the task read face only",
      ),
    );
  }
}
