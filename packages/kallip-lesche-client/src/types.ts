// Wire types for the `kallip-lesche` data-plane relay's `/v1` HTTP surface.
// These mirror the serde DTOs in
// `crates/platform/kallip-lesche-common/src/{control,message,event}.rs` and the
// lesche's `crates/platform/kallip-lesche/src/routes/conversations.rs`. The
// lesche forwards `Envelope.ciphertext` and the byte fields below without
// interpreting them; every base64 string is STANDARD base64 (padded, +//),
// matching agora-common's bytes.rs.

/** `POST /v1/conversations { tagma_id }` -- resolves the single conversation a
 * tagma owns with its operator (idempotent; id derived from the tagma). */
export interface CreateConversationResponse {
  readonly conversation_id: string;
}

/** Who sent an envelope. The relay is agent-free: an agent sender is attributed
 * only to its tagma. serde tag = `kind`, snake_case. */
export type Participant =
  | { readonly kind: "user"; readonly user_id: string }
  | { readonly kind: "agent"; readonly tagma_id: string };

/** The unit the lesche forwards. `sequence_n` is per-conversation, per-sender,
 * monotonic from 0; it doubles as the AEAD nonce counter. `ciphertext` is
 * standard-base64 AEAD output (ChaCha20-Poly1305, tag appended). `timestamp` is
 * ISO 8601 with fractional seconds (time::serde::iso8601); do not round-trip a
 * received value through `Date`. */
export interface Envelope {
  readonly conversation_id: string;
  readonly sender: Participant;
  readonly sequence_n: number;
  readonly trace_id: string;
  readonly timestamp: string;
  readonly ciphertext: string;
}

/** App -> tagma: one semantic op against the tagma, encrypted in an envelope.
 * serde tag = `op`, snake_case. `req_id` correlates the op with its TagmaReply. */
export type TagmaRequest =
  | {
      readonly op: "send_message";
      readonly req_id: number;
      readonly text: string;
    }
  | { readonly op: "interrupt"; readonly req_id: number };

/** App -> tagma: a control op that does NOT drive the agent (today: the
 * cursor-based history pull). Carried in the same encrypted envelope channel as
 * TagmaRequest; the relay dispatches by the `op` discriminant (disjoint from
 * TagmaRequest's). serde tag = `op`, snake_case. */
export type TagmaControl = {
  readonly op: "history";
  readonly req_id: number;
  /** rows with id > after (incremental catch-up). */
  readonly after: number | null;
  /** rows with id < before (scroll-up lazy load). */
  readonly before: number | null;
  readonly limit: number;
};

/** Why a failover chain ran out. Mirrors `event.rs::FailoverChainExhaustion`
 * (serde `rename_all = "camelCase"`). */
export type FailoverChainExhaustion =
  | "noFailoverConfigured"
  | "allBackupsExhausted"
  | "allCandidatesUnbuildable"
  | "allCandidatesInfeasible";

/** An event the tagma emits to the app (the agent-free subset of the tagma's
 * event stream, mapped by the tagma relay). serde tag = `type`, snake_case. There is
 * no streaming on this path: `assistant_content` is a complete message, and
 * `idle` is a content-less status transition. */
export type TagmaEvent =
  | { readonly type: "assistant_content"; readonly content: string }
  | { readonly type: "idle" }
  | { readonly type: "busy" }
  | { readonly type: "status"; readonly message: string }
  | { readonly type: "error"; readonly message: string }
  | { readonly type: "interrupted" }
  | { readonly type: "cancelled" }
  | {
      readonly type: "token_budget_exceeded";
      readonly consumed: number;
      readonly budget: number;
    }
  | { readonly type: "max_rounds_exceeded" }
  | {
      readonly type: "failover_chain_exhausted";
      readonly reason: FailoverChainExhaustion;
      readonly detail: string;
    };

/** Responder -> app: either the result of a correlated op, or an unsolicited
 * event from the tagma's event pump. serde tag = `kind`, snake_case. */
export type TagmaReply =
  | {
      readonly kind: "message_accepted";
      readonly req_id: number;
      readonly queue_depth: number;
      readonly warning?: string;
      /** `chat_history.id` of the **inbound** row the tagma appended for this
       * user message; the app stamps its optimistic local user line with this id
       * so it can be deduped against a later history replay. `0` (or absent)
       * means no row was recorded and must not be used for dedup. The ack itself
       * is never stored or replayed. */
      readonly history_id?: number;
      /** RFC 3339 send time of the inbound row. Absent on acks with no durable
       * row and on payloads serialized before the field existed. */
      readonly created_at?: string;
    }
  | { readonly kind: "interrupted"; readonly req_id: number }
  | {
      readonly kind: "error";
      readonly req_id: number;
      readonly status: number;
      readonly message: string;
    }
  | {
      readonly kind: "event";
      readonly event: TagmaEvent;
      /** `chat_history.id` of the outbound row the tagma appended for this event;
       * a stable id the app uses to order/dedup frames across batch replay and
       * live delivery. `0` (or absent) means the row was not recorded and must
       * not be used for dedup. */
      readonly history_id?: number;
      /** RFC 3339 send time of the outbound row. Absent on frames with no
       * durable row and on payloads serialized before the field existed. */
      readonly created_at?: string;
    }
  | {
      readonly kind: "user_message";
      readonly history_id: number;
      readonly text: string;
      /** RFC 3339 send time of the original inbound row. Absent on payloads
       * serialized before the field existed. */
      readonly created_at?: string;
    }
  | {
      readonly kind: "history_batch_end";
      readonly req_id: number;
      readonly count: number;
      readonly more: boolean;
    };

/** App -> tagma (relayed by the lesche): start a 1-RTT key exchange, carrying
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

/** An event on the app's multiplexed SSE stream (`GET /v1/me/events`). serde
 * tag = `type`, snake_case. `envelope` carries E2EE conversation content;
 * `tagma_online` / `tagma_offline` are the plaintext presence pair; `tagma_status`
 * is the tagma's periodic aggregate runtime snapshot (agent counts + token
 * budget), plaintext and user-scoped like presence. */
export type LescheEvent =
  | { readonly type: "envelope"; readonly envelope: Envelope }
  | { readonly type: "tagma_online"; readonly tagma_id: string }
  | { readonly type: "tagma_offline"; readonly tagma_id: string }
  | {
      readonly type: "tagma_status";
      readonly tagma_id: string;
      readonly root_state: "idle" | "busy" | "faulted";
      readonly subagents_total: number;
      readonly subagents_active: number;
      readonly token_budget: number;
      readonly token_consumed: number;
    };

/**
 * Lesche API error. Mirrors `kallip_common::protocol::ApiError`. This is a
 * distinct surface from `kallip-ui`'s tagma-transport `classifyError` -- the
 * lesche errors are routed through the realtime/channels stores, not the shared
 * AppShell banner.
 */
export class LescheApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "LescheApiError";
  }
}
