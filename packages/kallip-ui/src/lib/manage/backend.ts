// ManagementBackend: transport-agnostic interface for tagma management operations.
//
// Two implementations:
//   - OfflineBackend: wraps TagmaClient (HTTP to localhost tagma API)
//   - OnlineBackend: wraps RelayChannel.manage() (E2E encrypted channel)
//
// Both throw KallipError on non-2xx (OnlineBackend reconstructs it from the
// manage_result status+body) and TransportError on network/transport failures.

import type { TagmaClient } from "@kallipai/kallip-client";
import type { RelayChannel } from "@kallipai/kallip-lesche-client";
import {
  KallipError,
  type ApiError,
  TransportError,
} from "@kallipai/kallip-common";
import type {
  AgentStatusResponse,
  BudgetResponse,
  BudgetUpdateRequest,
  CreateWorkScheduleRequest,
  ListAgentsManagementResponse,
  ListAgentsQuery,
  ListWorkSchedulesQuery,
  ProfileApplyResponse,
  ProfileConfig,
  UpdateAgentMetadataRequest,
  UpdateDutyRequest,
  UpdateWorkScheduleRequest,
  WorkSchedule,
} from "@kallipai/kallip-client";

/** The 15 management methods shared by both backends. */
export interface ManagementBackend {
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
  updateProfiles(body: ProfileConfig): Promise<ProfileConfig>;
  applyProfiles(): Promise<ProfileApplyResponse>;
  listWorkSchedules(query?: ListWorkSchedulesQuery): Promise<WorkSchedule[]>;
  createWorkSchedule(body: CreateWorkScheduleRequest): Promise<WorkSchedule>;
  updateWorkSchedule(
    id: string,
    body: UpdateWorkScheduleRequest,
  ): Promise<WorkSchedule>;
  deleteWorkSchedule(id: string): Promise<void>;
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
  updateProfiles(body: ProfileConfig) {
    return this.client.updateProfiles(body);
  }
  applyProfiles() {
    return this.client.applyProfiles();
  }
  listWorkSchedules(query?: ListWorkSchedulesQuery) {
    return this.client.listWorkSchedules(query);
  }
  createWorkSchedule(body: CreateWorkScheduleRequest) {
    return this.client.createWorkSchedule(body);
  }
  updateWorkSchedule(id: string, body: UpdateWorkScheduleRequest) {
    return this.client.updateWorkSchedule(id, body);
  }
  deleteWorkSchedule(id: string) {
    return this.client.deleteWorkSchedule(id);
  }
}

// --- OnlineBackend (wraps RelayChannel.manage()) ---

/**
 * Reconstruct a KallipError from a manage_result when status >= 400. The tagma
 * returns its ApiError JSON in the loopback body; the IntoResponse extraction in
 * handle_manage wraps it as `{"error":{"message":"..."}}`.
 */
function parseError(status: number, body: unknown): Error {
  const envelope = body as { error?: { message?: string } };
  const message = envelope?.error?.message ?? "management request failed";
  return new KallipError({ status, message });
}

export class OnlineBackend implements ManagementBackend {
  constructor(private readonly channel: RelayChannel) {}

  private async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const result = await this.channel.manage(method, path, body ?? null);
    if (result.status >= 400) throw parseError(result.status, result.body);
    return result.body as T;
  }

  private async reqVoid(method: string, path: string, body?: unknown): Promise<void> {
    const result = await this.channel.manage(method, path, body ?? null);
    if (result.status >= 400) throw parseError(result.status, result.body);
  }

  getBudget() {
    return this.req<BudgetResponse>("GET", "/budget");
  }
  updateBudget(body: BudgetUpdateRequest) {
    return this.req<BudgetResponse>("POST", "/budget", body);
  }
  listAgents(query?: ListAgentsQuery) {
    const qs = query?.created_by
      ? `?created_by=${encodeURIComponent(query.created_by)}`
      : "";
    return this.req<ListAgentsManagementResponse>("GET", `/agents${qs}`);
  }
  getAgentStatus(id: string) {
    return this.req<AgentStatusResponse>("GET", `/agents/${id}/status`);
  }
  interruptAgent(id: string) {
    return this.reqVoid("POST", `/agents/${id}/interrupt`);
  }
  removeAgent(id: string) {
    return this.reqVoid("DELETE", `/agents/${id}`);
  }
  setAgentDuty(id: string, body: UpdateDutyRequest) {
    return this.reqVoid("PUT", `/agents/${id}/duty`, body);
  }
  updateAgentMetadata(id: string, body: UpdateAgentMetadataRequest) {
    return this.reqVoid("PUT", `/agents/${id}/metadata`, body);
  }
  getProfiles() {
    return this.req<ProfileConfig>("GET", "/profiles");
  }
  updateProfiles(body: ProfileConfig) {
    return this.req<ProfileConfig>("PUT", "/profiles", body);
  }
  applyProfiles() {
    return this.req<ProfileApplyResponse>("POST", "/profiles/apply");
  }
  listWorkSchedules(query?: ListWorkSchedulesQuery) {
    const params = new URLSearchParams();
    if (query?.agent_id) params.set("agent_id", query.agent_id);
    if (query?.status) params.set("status", query.status);
    const qs = params.toString();
    return this.req<WorkSchedule[]>("GET", `/work-schedules${qs ? `?${qs}` : ""}`);
  }
  createWorkSchedule(body: CreateWorkScheduleRequest) {
    return this.req<WorkSchedule>("POST", "/work-schedules", body);
  }
  updateWorkSchedule(id: string, body: UpdateWorkScheduleRequest) {
    return this.req<WorkSchedule>("PUT", `/work-schedules/${id}`, body);
  }
  deleteWorkSchedule(id: string) {
    return this.reqVoid("DELETE", `/work-schedules/${id}`);
  }
}
