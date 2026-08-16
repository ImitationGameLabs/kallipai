// Wire types for the tagma HTTP surface the browser frontend consumes. The
// counterpart Rust serde DTOs live in kallip-common and kallip-tagma; field
// names are snake_case here (matching serde), and every base64 string is
// STANDARD base64 (padded, +//).

import type { HistoryEntry } from "@kallipai/kallip-common";

/** `GET /agents/root` -- the tagma's single root agent (always present after
 * startup). `id` binds the transport; `conversation_id` (present only on the
 * root summary, when the tagma is enrolled) is the shared key the offline and
 * online paths use for the IndexedDB cache + history pulls. */
export interface WireAgentSummary {
  readonly id: string;
  readonly workspace_root?: string;
  readonly state: "idle" | "busy" | "faulted";
  readonly created_by?: string;
  readonly role: string;
  readonly description?: string;
  readonly activity?: string;
  readonly faulted_reason?: string | null;
  readonly conversation_id?: string;
}

/** `POST /agents/{id}/message` -- queue-depth feedback for an inbound user
 * message. The direct path carries no `message_accepted` ack, so this is the
 * only response the send path observes. */
export interface MessageResponse {
  readonly queue_depth: number;
  readonly warning?: string;
}

/** `GET /agents/{id}/external/history` -- a cursor-driven history window for
 * the direct (offline) path. `rows` are decoded `HistoryEntry` frames (the
 * sender paired with an authored `event` / `user_message` echo); `more` is true
 * only for paginated (`after`/`before`) queries that returned a full page.
 * Mirrors the relay `TagmaControl::History` shape. */
export interface ExternalHistoryResponse {
  /** History entries: the sender paired with the content-only reply (mirrors
   * the live `{sender, body}` shape). */
  readonly rows: readonly HistoryEntry[];
  readonly more: boolean;
}

// --- Management API wire types ---
// All verified against Rust source. Budget/token fields are u64 numbers (safe
// for JS Number within practical ranges).

// Budget

/** `GET /budget` / `POST /budget` response. */
export interface BudgetResponse {
  readonly budget: number;
  readonly consumed: number;
  readonly remaining: number;
}

/** `POST /budget` request body — exactly one of set_remaining or delta. */
export interface BudgetUpdateRequest {
  readonly set_remaining?: number;
  readonly delta?: number;
}

// Agent management

/** Agent list item (`GET /agents`). No token/budget fields. */
export interface WireAgentManagementSummary {
  readonly id: string;
  readonly workspace_root: string;
  readonly state: "idle" | "busy" | "faulted";
  readonly created_by: string | null;
  readonly role: string;
  readonly description: string;
  readonly activity: string;
  readonly duty: "onduty" | "offduty";
  readonly faulted_reason: string | null;
  readonly conversation_id: string | null;
}

/** `GET /agents` response wrapper. */
export interface ListAgentsManagementResponse {
  readonly agents: readonly WireAgentManagementSummary[];
}

/** Cumulative token usage per agent. */
export interface CumulativeUsage {
  readonly prompt_tokens: number;
  readonly completion_tokens: number;
  readonly cache_hit_tokens: number;
}

/** Context usage snapshot (`AgentStatusResponse.context`). */
export interface ContextUsage {
  readonly pinned_items: readonly [string, number][];
  readonly turn_count: number;
  readonly turn_tokens: number;
  readonly last_prompt_tokens: number | null;
  readonly cumulative_usage: CumulativeUsage;
}

/** Retry record (`AgentStatusResponse.recent_retries`). */
export interface RetryRecord {
  readonly timestamp: number;
  readonly round: number;
  readonly attempt: number;
  readonly max_attempts: number;
  readonly error: string;
  readonly delay_secs: number;
  readonly endpoint: string | null;
}

/** `GET /agents/{id}/status` response. token_budget/token_consumed are tagma-wide. */
export interface AgentStatusResponse {
  readonly state: "idle" | "busy" | "faulted";
  readonly context: ContextUsage;
  readonly recent_retries: readonly RetryRecord[];
  readonly token_budget: number;
  readonly token_consumed: number;
  readonly activity: string;
}

/** `PUT /agents/{id}/metadata` request body. */
export interface UpdateAgentMetadataRequest {
  readonly role?: string;
  readonly description?: string;
}

/** `PUT /agents/{id}/duty` request body. */
export interface UpdateDutyRequest {
  readonly status: "onduty" | "offduty";
}

/** `GET /agents` list query. */
export interface ListAgentsQuery {
  readonly created_by?: string;
}

// Profiles

/** Provider endpoint (credentials + optional base URL). GET returns a masked
 * api_key; on PUT null keeps the live key. */
export interface ProfileEndpoint {
  readonly id: string;
  readonly family: string;
  readonly api_key: string | null;
  readonly base_url: string | null;
}

/** A model bound to an endpoint. */
export interface ProfileModel {
  readonly id: string;
  readonly endpoint: string;
  readonly model: string;
  readonly max_context_window: number;
}

/** A capability tier — purely positional (tiers[depth]). */
export interface ProfileTier {
  readonly profiles: readonly ProfileModel[];
}

/** `GET /profiles` / `PUT /profiles` body. */
export interface ProfileConfig {
  readonly tiers: readonly ProfileTier[];
  readonly endpoints: Readonly<Record<string, ProfileEndpoint>>;
}

/** `POST /profiles/apply` response. */
export interface ProfileApplyResponse {
  readonly applied: number;
  readonly skipped: number;
}

// Profile probe (dry-run validation before applying)

/** Per-endpoint definition sent to POST /profiles/probe. `api_key: null` reuses the live key. */
export interface ProfileProbeEndpoint {
  readonly id: string;
  readonly family: string;
  readonly api_key: string | null;
  readonly base_url: string | null;
}

/** Per-profile model reference inside a probed tier. */
export interface ProfileProbeProfile {
  readonly id: string;
  readonly endpoint: string;
  readonly model: string;
}

/** `POST /profiles/probe` request. */
export interface ProfileProbeRequest {
  readonly endpoints: readonly ProfileProbeEndpoint[];
  readonly tiers: readonly {
    readonly profiles: readonly ProfileProbeProfile[];
  }[];
}

export type ProfileProbeStatus =
  | "ok"
  | "unreachable"
  | "unauthorized"
  | "invalid_config"
  | "partial";

/** Probe outcome for one endpoint: catalog/balance info on success, reason otherwise. */
export interface ProfileProbeEndpointReport {
  readonly endpoint_id: string;
  readonly status: ProfileProbeStatus;
  readonly latency_ms: number | null;
  readonly catalog_count: number | null;
  readonly models: readonly string[] | null;
  readonly balance: unknown;
  readonly detail: string | null;
}

/** Probe outcome for one profile (model reference) inside a tier. */
export interface ProfileProbeProfileReport {
  readonly profile_id: string;
  readonly endpoint_id: string;
  readonly status: ProfileProbeStatus;
  readonly detail: string | null;
}

/** Probe rollup for one tier. */
export interface ProfileProbeTierReport {
  readonly index: number;
  readonly all_ok: boolean;
  readonly profiles: readonly ProfileProbeProfileReport[];
}

/** `POST /profiles/probe` response. */
export interface ProfileProbeResponse {
  readonly results: readonly ProfileProbeEndpointReport[];
  readonly tiers: readonly ProfileProbeTierReport[];
}

// Work schedules

/** A work schedule definition. */
export interface WorkSchedule {
  readonly id: string;
  readonly name: string;
  readonly agent_id: string;
  readonly start_cron: string;
  readonly end_cron: string;
  readonly pre_warn_minutes: number;
  readonly final_warn_minutes: number;
  readonly wake_prompt: string;
  readonly status: "active" | "paused";
  readonly timezone: string | null;
  readonly created_at: string;
}

/** `POST /work-schedules` request body. */
export interface CreateWorkScheduleRequest {
  readonly name: string;
  readonly agent_id: string;
  readonly start_cron: string;
  readonly end_cron: string;
  readonly pre_warn_minutes?: number;
  readonly final_warn_minutes?: number;
  readonly wake_prompt: string;
  readonly status?: "active" | "paused";
  readonly timezone?: string | null;
}

/** `PUT /work-schedules/{id}` request body (all fields optional). */
export interface UpdateWorkScheduleRequest {
  readonly name?: string;
  readonly start_cron?: string;
  readonly end_cron?: string;
  readonly pre_warn_minutes?: number;
  readonly final_warn_minutes?: number;
  readonly wake_prompt?: string;
  readonly status?: "active" | "paused";
  readonly timezone?: string | null;
}

/** `GET /work-schedules` query params. */
export interface ListWorkSchedulesQuery {
  readonly agent_id?: string;
  readonly status?: "active" | "paused";
}
