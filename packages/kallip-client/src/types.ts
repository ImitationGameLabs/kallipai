// Wire types for the tagma HTTP surface the browser frontend consumes. The
// counterpart Rust serde DTOs live in kallip-common and kallip-tagma; field
// names are snake_case here (matching serde), and every base64 string is
// STANDARD base64 (padded, +//).

import type { HistoryEntry } from "@kallipai/kallip-common";
/** Agent lifecycle state, serialized snake_case by the tagma's `AgentState`
 * enum. All six values reach every state-bearing surface (agent list
 * summaries, status responses, the realtime `tagma_status` snapshot), so
 * front-end unions copy this list rather than narrowing it. */
export type AgentState =
  | "idle"
  | "busy"
  | "waiting"
  | "retrying"
  | "parked"
  | "faulted";

/** Why a parked agent parked: the tagma's `ParkedReason`, externally
 * tagged with camelCase variant keys; unit variants arrive as `null`. */
export type WireParkedReason =
  | {
      readonly failoverChainExhausted: {
        readonly reason: string;
        readonly detail: string;
      };
    }
  | { readonly fatalError: { readonly message: string } }
  | {
      readonly tokenBudgetExceeded: {
        readonly consumed: number;
        readonly budget: number;
      };
    }
  | { readonly maxRoundsExceeded: null }
  | { readonly transientRetryExhausted: null };

/** Armed chain-transient retry counters (`retrying` on summaries and
 * status responses). `retry_in_secs` is the relative backoff delay. */
export interface WireTransientRetryInfo {
  readonly attempt: number;
  readonly max_attempts: number;
  readonly retry_in_secs: number;
}

/** `GET /agents/root` -- the tagma's single root agent (always present after
 * startup). `id` binds the transport; `conversation_id` (present only on the
 * root summary, when the tagma is enrolled) is the shared key the offline and
 * online paths use for the IndexedDB cache + history pulls. */
export interface WireAgentSummary {
  readonly id: string;
  readonly workspace_root?: string;
  readonly state: AgentState;
  readonly created_by?: string;
  readonly role: string;
  readonly description?: string;
  readonly activity?: string;
  readonly faulted_reason?: string | null;
  /** Present only when `state == "parked"`: why the agent parked (absent
   * otherwise; serde skips `None`). */
  readonly parked_reason?: WireParkedReason | null;
  /** Present only when `state == "retrying"`: armed backoff counters. */
  readonly retrying?: WireTransientRetryInfo | null;
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
  readonly state: AgentState;
  readonly created_by: string | null;
  readonly role: string;
  readonly description: string;
  readonly activity: string;
  readonly duty: "onduty" | "offduty";
  readonly faulted_reason: string | null;
  /** Present only when `state == "parked"`: why (see `WireParkedReason`). */
  readonly parked_reason?: WireParkedReason | null;
  /** Present only when `state == "retrying"`: armed backoff counters. */
  readonly retrying?: WireTransientRetryInfo | null;
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

/** Runtime-active model profile (`AgentStatusResponse.profile`): the id of the
 * registry profile in use, its provider endpoint, its tier position, and the
 * concrete model string the client sends. */
export interface ActiveProfile {
  /** Positional tier index in the registry (0-based). */
  readonly tier_index: number;
  readonly profile_id: string;
  /** The endpoint (provider) id this profile connects through. */
  readonly provider: string;
  /** For env-configured single-profile agents this is the raw
   * KALLIP_LLM_MODEL value, so an env-only setup still shows a
   * meaningful model string. */
  readonly model: string;
}
/** `GET /agents/{id}/status` response. token_budget/token_consumed are tagma-wide. */
export interface AgentStatusResponse {
  readonly state: AgentState;
  readonly context: ContextUsage;
  readonly recent_retries: readonly RetryRecord[];
  readonly token_budget: number;
  readonly token_consumed: number;
  readonly activity: string;
  /** Present only when `state == "parked"`: why (see `WireParkedReason`). */
  readonly parked_reason?: WireParkedReason | null;
  /** Present only when `state == "retrying"`: armed backoff counters. */
  readonly retrying?: WireTransientRetryInfo | null;
  /** Omitted only by a tagma that predates the field. */
  readonly profile?: ActiveProfile;
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

/** Provider (credentials + optional base URL). GET returns a masked
 * api_key; on PUT null keeps the live key. */
export interface ProfileProvider {
  readonly id: string;
  readonly family: string;
  readonly api_key: string | null;
  readonly base_url: string | null;
}

/** A model bound to a provider. */
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
  readonly endpoints: Readonly<Record<string, ProfileProvider>>;
  /** Profiles parked out of rotation (draft space, absent = empty).
   * GET omits the key when empty; on PUT an absent key keeps the live
   * parking (tri-state — see the Rust merge), while the UI always sends it.
   */
  readonly parking?: readonly ProfileModel[];
}

/** `POST /profiles/apply` response. */
export interface ProfileApplyResponse {
  readonly applied: number;
  readonly skipped: number;
}

// Profile probe (dry-run validation before applying)

/** Per-provider definition sent to POST /profiles/probe. `api_key: null` reuses the live key. */
export interface ProfileProviderProbeRequest {
  readonly id: string;
  readonly family: string;
  readonly api_key: string | null;
  readonly base_url: string | null;
}

/** Per-profile model reference inside a probed tier. */
export interface ProfileModelProbeRequest {
  readonly id: string;
  readonly endpoint: string;
  readonly model: string;
}

/** `POST /profiles/probe` request. */
export interface ProfileProbeRequest {
  readonly endpoints: readonly ProfileProviderProbeRequest[];
  readonly tiers: readonly {
    readonly profiles: readonly ProfileModelProbeRequest[];
  }[];
}

export type ProfileProbeStatus =
  | "ok"
  | "unreachable"
  | "unauthorized"
  | "invalid_config"
  | "partial";

/** Probe outcome for one endpoint: catalog/balance info on success, reason otherwise. */
export interface ProfileProviderProbeReport {
  readonly endpoint_id: string;
  readonly status: ProfileProbeStatus;
  readonly latency_ms: number | null | undefined;
  readonly catalog_count: number | null | undefined;
  readonly models: readonly string[] | null | undefined;
  readonly balance: unknown;
  readonly detail: string | null | undefined;
}

/** Probe outcome for one profile (model reference) inside a tier. */
export interface ProfileModelProbeReport {
  readonly profile_id: string;
  readonly endpoint_id: string;
  readonly status: ProfileProbeStatus;
  readonly detail: string | null | undefined;
}

/** Probe rollup for one tier. */
export interface ProfileTierProbeReport {
  readonly index: number;
  readonly all_ok: boolean;
  readonly profiles: readonly ProfileModelProbeReport[];
}

/** `POST /profiles/probe` response. */
export interface ProfileProbeResponse {
  readonly results: readonly ProfileProviderProbeReport[];
  readonly tiers: readonly ProfileTierProbeReport[];
}

// Work schedules

/** One duty window inside a weekly/monthly spec; window semantics
 * are documented on `WorkScheduleSpec`. */
export interface WorkScheduleWindow {
  readonly start_minute: number;
  readonly end_minute: number;
}

/**
 * Work-schedule spec: the structured form the UI edits and the evaluator
 * consumes. All times are UTC. Minute-of-day windows are half-open
 * [start, end); `end_minute == 1440` is a full-day window, and an end at
 * or below the start crosses midnight, belonging to the start day.
 */
export type WorkScheduleSpec =
  | {
      readonly mode: "weekly";
      /** Bitmask: bit 0 = Monday … bit 6 = Sunday. */
      readonly days: number;
      readonly windows: readonly WorkScheduleWindow[];
    }
  | {
      readonly mode: "monthly";
      /** Bitmask: bit 0 = the 1st … bit 30 = the 31st. */
      readonly days: number;
      readonly windows: readonly WorkScheduleWindow[];
    }
  | {
      readonly mode: "interval";
      /** Rotation period in hours; the rhythm runs across day boundaries. */
      readonly every_hours: number;
      readonly length_min: number;
      /**
       * RFC3339, minute-aligned. Re-anchored server-side whenever the
       * rhythm (every_hours/length_min) changes; unchanged rhythms keep it.
       */
      readonly anchor: string;
    }
  | {
      /** 24/7 duty: phase-free, so no clock view can misread it. */
      readonly mode: "always";
    };

/** The tagma's single work schedule. */
export interface WorkSchedule {
  readonly id: string;
  readonly spec: WorkScheduleSpec;
  readonly pre_warn_minutes: number;
  readonly final_warn_minutes: number;
  readonly wake_prompt: string;
  /** Custom final-warn text; null keeps the built-in default. */
  readonly final_warn_prompt: string | null;
  readonly status: "active" | "paused";
  readonly created_at: string;
}

/** `PUT /work-schedule` request body; the first PUT creates the schedule. */
export interface PutWorkScheduleRequest {
  readonly spec: WorkScheduleSpec;
  readonly pre_warn_minutes?: number;
  readonly final_warn_minutes?: number;
  readonly wake_prompt?: string;
  /** Absent keeps the stored value; "" clears back to the default. */
  readonly final_warn_prompt?: string;
  readonly status?: "active" | "paused";
}
