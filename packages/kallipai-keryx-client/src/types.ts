// Wire types for the `kallipai-keryx` data-plane relay's `/` HTTP surface:
// the bilateral (1:1) E2EE conversation family plus the tagma projection
// read face. These mirror the serde DTOs in
// `crates/platform/kallipai-keryx-common/src/{control,event,projection}.rs`
// and the keryx's routes. The keryx forwards `Envelope.ciphertext` and the
// byte fields below without interpreting them; every base64 string is
// STANDARD base64 (padded, +//), matching archeion-common's bytes.rs.
//
// The chat wire types (`AuthoredEvent`, `SignalEvent`, `TagmaReply`,
// `Envelope`, `TagmaStatusPayload`) live in `@kallipai/kallipai-common` and
// are re-exported here, because the direct (offline) client shares them --
// the external chat vocabulary is transport-agnostic.

// Re-export the shared chat vocabulary (one source of truth in
// kallipai-common).
export type {
  AuthoredEvent,
  Envelope,
  FailoverChainExhausted,
  FileAttachment,
  HistoryEntry,
  Participant,
  ParticipantKind,
  SignalEvent,
  TagmaReply,
  TagmaStatusPayload,
} from "@kallipai/kallipai-common";
// The data-plane error class is shared with the lesche client (both
// services emit the same error envelope); it lives in kallipai-common.
export { DataPlaneApiError } from "@kallipai/kallipai-common";
import type {
  Envelope,
  FileAttachment,
  SignalEvent,
  TagmaStatusPayload,
} from "@kallipai/kallipai-common";

/** `POST /conversations { tagma_id }` -- resolves the single conversation a
 * tagma owns with its operator (idempotent; id derived from the tagma). */
export interface CreateConversationResponse {
  readonly conversation_id: string;
}

/** App -> tagma: one semantic op against the tagma, encrypted in an envelope.
 * serde tag = `op`, snake_case. `req_id` correlates the op with its TagmaReply. */
export type TagmaRequest =
  | {
      readonly op: "send_message";
      readonly req_id: number;
      readonly text: string;
      readonly attachment?: FileAttachment;
      /** The 1:1 history row id this message quotes (`send_message` op). An
       * id, never the quoted text: the tagma resolves it against its own
       * history, and a row that is gone degrades to a placeholder. */
      readonly quote?: number;
    }
  | { readonly op: "interrupt"; readonly req_id: number };

/** App -> tagma: a control op that does NOT drive the agent (today: the
 * cursor-based history pull). Carried in the same encrypted envelope channel as
 * TagmaRequest; the relay dispatches by the `op` discriminant (disjoint from
 * TagmaRequest's). serde tag = `op`, snake_case. */
export type TagmaControl =
  | {
      readonly op: "history";
      readonly req_id: number;
      /** rows with id > after (incremental catch-up). */
      readonly after: number | null;
      /** rows with id < before (scroll-up lazy load). */
      readonly before: number | null;
      readonly limit: number;
    }
  | {
      /** A management operation (budget, agents, profiles, schedules). The
       * relay dispatches it in-process against the tagma route handlers. */
      readonly op: "manage";
      readonly req_id: number;
      readonly method: string;
      readonly path: string;
      readonly body: unknown;
    };

/** App -> tagma (relayed by the keryx): start a 1-RTT key exchange, carrying
 * the app's ephemeral X25519 public key (standard base64). */
export interface KeyExchangeInit {
  readonly ephemeral_public: string;
}

/** Responder -> app: the tagma's ephemeral X25519 public key plus an Ed25519
 * signature over the kex transcript (standard base64). */
export interface KeyExchangeResponse {
  readonly ephemeral_public: string;
  readonly signature: string;
}

/** An event on the keryx `/me/events` stream. Internally tagged like the
 * lesche sibling (`type` + snake_case) with identical variant names and
 * field shapes for the shared family: the split changes who emits the
 * frames, not their bytes. This stream carries the bilateral (1:1)
 * conversation family (envelopes, the tagma presence pair, status and
 * signal, unread) plus `file_delivered` (the files push, which only the
 * emits). The room family stays on the lesche stream. */
export type KeryxEvent =
  | { readonly type: "envelope"; readonly envelope: Envelope }
  | { readonly type: "tagma_online"; readonly tagma_id: string }
  | { readonly type: "tagma_offline"; readonly tagma_id: string }
  | ({
      readonly type: "tagma_status";
      readonly tagma_id: string;
    } & TagmaStatusPayload)
  | {
      readonly type: "tagma_signal";
      readonly tagma_id: string;
      readonly event: SignalEvent;
    }
  | {
      // The tagma's unread count for this conversation: outbound rows
      // past the user's read cursor, stamped by the tagma. The client
      // assigns the number, it never derives one of its own.
      readonly type: "tagma_unread";
      readonly tagma_id: string;
      readonly peer: string;
      readonly cursor: number;
      readonly unread: number;
    }
  | {
      // A file landed in this user's files space (the delivery side of
      // the files service's send). The recipient fetches the content
      // itself via GET /files/{record_id}; the files listing is the
      // resync ground truth, so a dropped frame only delays discovery.
      readonly type: "file_delivered";
      readonly record_id: string;
      readonly path: string;
      readonly from: string;
      readonly name: string;
      readonly size: number;
    };

/**
 * One `ProjectionDirty` frame on the projection SSE stream
 * (`GET /tagmata/{id}/state`). A liveness nudge only -- the
 * payload carries the tagma id and the store seq, never content; the
 * consumer re-pulls the projection GET when the seq is newer than its
 * local copy.
 */
export interface ProjectionDirty {
  readonly tagma_id: string;
  readonly seq: number;
}

/** `GET /tagmata/{id}/agents` -- the stored projection's
 * roster and aggregate status, plus the seq/staleness bookkeeping.
 * `stale` is true when the tagma has no live presence (offline
 * tags keep serving the last known projection). */
export interface ProjectionAgentsResponse {
  readonly stale: boolean;
  readonly seq: number;
  readonly updated_at: number;
  readonly agents: readonly ProjectionAgentSummary[];
  readonly status: unknown;
}

/** The projected per-agent summary: the same serde wire as kallipai-ui's
 * management summary common fields (structural, so the UI consumes it
 * directly; extra wire fields the management type omits, like `lock`,
 * are harmless). */
export interface ProjectionAgentSummary {
  readonly id: string;
  readonly workspace_root: string;
  readonly state:
    | "idle"
    | "busy"
    | "waiting"
    | "parked"
    | "retrying"
    | "faulted";
  readonly created_by: string | null;
  readonly role: string;
  readonly description: string;
  readonly activity: string;
  readonly duty: "onduty" | "offduty";
  readonly faulted_reason: string | null;
  readonly parked_reason?: unknown;
  readonly retrying?: unknown;
  readonly conversation_id: string | null;
  readonly profile_set?: string | null;
}

/** `GET /tagmata/{id}/budget` -- the cached token budget
 * snapshot with the same seq/staleness bookkeeping. */
export interface ProjectionBudgetResponse {
  readonly stale: boolean;
  readonly seq: number;
  readonly updated_at: number;
  readonly budget: number;
  readonly consumed: number;
}

/** `GET /tagmata/{id}/work-schedule` -- the cached
 * prompt-free schedule projection (same seq/staleness bookkeeping). */
export interface ProjectionWorkScheduleResponse {
  readonly stale: boolean;
  readonly seq: number;
  readonly updated_at: number;
  readonly work_schedule: {
    readonly id: string;
    readonly spec: unknown;
    readonly pre_warn_minutes: number;
    readonly final_warn_minutes: number;
    readonly status: string;
    readonly created_at: string;
  } | null;
}

/** `GET /tagmata/{id}/status` -- the presence cache's latest relayed snapshot
 * (`stale: false`), or an offline tagma's stored projection (`stale: true`).
 * `status` is `null` when the tagma has never broadcast a snapshot. */
export interface ProjectionStatusResponse {
  readonly stale: boolean;
  readonly status: TagmaStatusPayload | null;
}
