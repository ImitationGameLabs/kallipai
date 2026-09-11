// Wire types for the tagma HTTP surface the browser frontend consumes. The
// counterpart Rust serde DTOs live in kallip-common and kallip-tagma; field
// names are snake_case here (matching serde), and every base64 string is
// STANDARD base64 (padded, +//).

import type {
  FileAttachment,
  HistoryEntry,
  Participant,
} from "@kallipai/kallip-common";
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
  /** Workspace write-lock visibility (`held`/`missing`), present only for a
   * live normal-class agent (serde skips `None`): `missing` there is the
   * lock-evaporation red flag. */
  readonly lock?: "held" | "missing" | null;
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
  readonly unlimited: boolean;
}

/** `POST /budget` request body — exactly one of set_remaining, delta, or set_unlimited. */
export interface BudgetUpdateRequest {
  readonly set_remaining?: number;
  readonly delta?: number;
  readonly set_unlimited?: boolean;
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
  /** The named profile set this agent is bound to (omitted = none; the
   * Rust side skips the field on `None`). */
  readonly profile_set?: string | null;
}

/** `GET /agents` response wrapper. */
export interface ListAgentsManagementResponse {
  readonly agents: readonly WireAgentManagementSummary[];
}

/** Cumulative token usage per agent. */
export interface CumulativeUsage {
  readonly prompt_tokens: number;
  readonly completion_tokens: number;
  readonly cache_read_tokens: number;
  readonly cache_write_tokens: number;
}

/** Context usage snapshot (`AgentStatusResponse.context`). */
export interface ContextUsage {
  readonly pinned_items: readonly [string, number][];
  readonly turn_count: number;
  readonly turn_tokens: number;
  readonly last_prompt_tokens: number | null;
  readonly cumulative_usage: CumulativeUsage;
  readonly largest_turns: readonly [number, number][];
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
 * registry profile in use, its provider endpoint, the recorded profile-set
 * name, and the concrete model string the client sends. */
export interface ActiveProfile {
  /** The recorded profile-set name this agent resolves against. */
  readonly profile_set: string;
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

/** Reasoning effort levels a profile may request; forwarded to providers as-is. */
export type ReasoningEffort = "low" | "medium" | "high" | "xhigh" | "max";
/** Content modalities a profile may serve; a profile set's effective
 * modalities are the intersection across its member profiles. */
export type Modality = "text" | "image" | "audio" | "video";

/** A model bound to a provider. */
export interface ProfileModel {
  readonly id: string;
  readonly endpoint: string;
  readonly model: string;
  readonly max_context_window: number;
  /** Absent = the client default (response storage on). */
  readonly store?: boolean;
  /** Absent = no effort level requested. */
  readonly effort?: ReasoningEffort;
  /** Modalities the profile declares it can serve; absent = text-only. */
  readonly modalities?: readonly Modality[];
}

/** A named profile set: an ordered failover chain. The map key in
 * `ProfileConfig.sets` carries the name. */
export interface ProfileSet {
  readonly description: string | null;
  readonly profiles: readonly ProfileModel[];
}

/** `GET /profiles` response body. */
export interface ProfileConfig {
  readonly sets: Readonly<Record<string, ProfileSet>>;
  /** The default set's name (GET omits it when no sets exist). */
  readonly default?: string;
  readonly endpoints: Readonly<Record<string, ProfileProvider>>;
  /** Profiles parked out of rotation (draft space, absent = empty).
   * GET omits the key when empty; on PUT an absent key keeps the live
   * parking (tri-state — see the Rust merge), while the UI always sends it.
   */
  readonly parking?: readonly ProfileModel[];
}

/** One named set inside a `PUT /profiles` body: sets serialize as an
 * array (GET maps them by name), so each element carries its name
 * explicitly. */
export interface ProfileSetPutRequest {
  readonly name: string;
  readonly description: string | null;
  readonly profiles: readonly ProfileModel[];
}

/** `PUT /profiles` request body: sets serialize as a name-carrying array.
 * The tri-state fields mirror the Rust merge: an absent `default` lets the
 * server resolve it, an absent `parking` keeps the live list (the UI
 * always sends parking explicitly), and a null endpoint `api_key` keeps
 * the live key. */
export interface ProfileConfigPutRequest {
  readonly sets: readonly ProfileSetPutRequest[];
  readonly default?: string;
  readonly endpoints: Readonly<Record<string, ProfileProvider>>;
  readonly parking: readonly ProfileModel[];
  /** Operator-confirmed acceptance of dangling profile-set bindings (the
   * server's 409 lists them; absent/false keeps the hard reject). */
  readonly force?: boolean;
}
/** `POST /profiles/apply` response. */
export interface ProfileApplyResponse {
  readonly applied: number;
  readonly skipped: number;
}

/** One agent still bound to a set — the reference list a set deletion must
 * clear (by interrupt) before the set can be removed. */
export interface SetReference {
  readonly id: string;
  readonly role?: string;
}

/** `DELETE /profiles/sets/{name}` response. */
export interface DeleteSetResponse {
  readonly removed: string;
  readonly interrupted: readonly SetReference[];
}

// Profile probe (dry-run validation before applying)

/** Per-provider definition sent to POST /profiles/probe. `api_key: null` reuses the live key. */
export interface ProfileProviderProbeRequest {
  readonly id: string;
  readonly family: string;
  readonly api_key: string | null;
  readonly base_url: string | null;
}

/** Per-profile model reference inside a probed set. */
export interface ProfileModelProbeRequest {
  readonly id: string;
  readonly endpoint: string;
  readonly model: string;
}

/** `POST /profiles/probe` request. */
export interface ProfileProbeRequest {
  readonly endpoints: readonly ProfileProviderProbeRequest[];
  readonly sets: readonly {
    readonly name: string;
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

/** Probe outcome for one profile (model reference) inside a set. */
export interface ProfileModelProbeReport {
  readonly profile_id: string;
  readonly endpoint_id: string;
  readonly status: ProfileProbeStatus;
  readonly detail: string | null | undefined;
}

/** Probe rollup for one set. */
export interface ProfileSetProbeReport {
  readonly name: string;
  readonly all_ok: boolean;
  readonly profiles: readonly ProfileModelProbeReport[];
}

/** `POST /profiles/probe` response. */
export interface ProfileProbeResponse {
  readonly results: readonly ProfileProviderProbeReport[];
  readonly sets: readonly ProfileSetProbeReport[];
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
  /** Custom wake text appended after the built-in default; "" is the default alone. */
  readonly wake_prompt: string;
  /** Custom final-warn text appended after the built-in default; null is the default alone. */
  readonly final_warn_prompt: string | null;
  readonly status: "active" | "paused";
  readonly created_at: string;
}

/** `PUT /work-schedule` request body; the first PUT creates the schedule. */
export interface PutWorkScheduleRequest {
  readonly spec: WorkScheduleSpec;
  readonly pre_warn_minutes?: number;
  readonly final_warn_minutes?: number;
  /** Absent keeps the stored value; "" stores "" (the default alone). */
  readonly wake_prompt?: string;
  /** Absent keeps the stored value; "" clears back to the default. */
  readonly final_warn_prompt?: string;
  readonly status?: "active" | "paused";
}

// --- Lesche session surfaces (the root agent's relay conversation data) ---
// All verified against the Rust handlers (kallip-tagma routes/lesche.rs).
// The `format=json` history variant is the console UI's transcript contract;
// the no-param text render stays the CLI/prompt contract.

/** One row of `GET /agents/{id}/lesche/sessions`: every surface the agent
 * can address with `kallip lesche send`, with its kind and target metadata. */
export interface LescheSessionEntry {
  /** `bilateral` (the fixed 1:1 with the operator), `room`, or `direct`. */
  readonly kind: "bilateral" | "room" | "direct";
  /** The surface id: conversation id, room id, or derived session id. */
  readonly id: string;
  /** Room display name (rooms only). */
  readonly name?: string;
  /** The peer's tagma id (direct sessions only). */
  readonly peer_tagma?: string;
  /** The peer's server-stamped handle (direct sessions only). */
  readonly peer_handle?: string;
}

/** One row of the direct-session history read in its `format=json` variant:
 * the decoded payload flattened to the top level next to the row's envelope
 * metadata. `created_at` is ISO 8601. */
export interface DirectMessageRow {
  readonly seq: number;
  readonly sender: Participant;
  readonly text: string;
  /** Reference-style attachment: the bytes stay in the files service; the
   * peer fetches `record_id` in its own authorized space. */
  readonly attachment?: FileAttachment;
  readonly created_at: string;
}
