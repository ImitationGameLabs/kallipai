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
  ListAgentsManagementResponse,
  ListAgentsQuery,
  ProfileApplyResponse,
  ProfileConfig,
  ProfileProbeRequest,
  ProfileProbeResponse,
  PutWorkScheduleRequest,
  UpdateAgentMetadataRequest,
  UpdateDutyRequest,
  WorkSchedule,
} from "@kallipai/kallip-client";

/** The 13 management methods shared by both backends. */
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
  probeProfiles(body: ProfileProbeRequest): Promise<ProfileProbeResponse>;
  getWorkSchedule(): Promise<WorkSchedule>;
  putWorkSchedule(body: PutWorkScheduleRequest): Promise<WorkSchedule>;
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

  probeProfiles(body: ProfileProbeRequest) {
    return this.client.probeProfiles(body);
  }
  getWorkSchedule() {
    return this.client.getWorkSchedule();
  }
  putWorkSchedule(body: PutWorkScheduleRequest) {
    return this.client.putWorkSchedule(body);
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

  private async req<T>(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<T> {
    const result = await this.channel.manage(method, path, body ?? null);
    if (result.status >= 400) throw parseError(result.status, result.body);
    return result.body as T;
  }

  private async reqVoid(
    method: string,
    path: string,
    body?: unknown,
  ): Promise<void> {
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

  probeProfiles(body: ProfileProbeRequest) {
    return this.req<ProfileProbeResponse>("POST", "/profiles/probe", body);
  }
  getWorkSchedule() {
    return this.req<WorkSchedule>("GET", "/work-schedule");
  }
  putWorkSchedule(body: PutWorkScheduleRequest) {
    return this.req<WorkSchedule>("PUT", "/work-schedule", body);
  }
}
