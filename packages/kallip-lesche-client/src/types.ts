// Wire types for the `kallip-lesche` data-plane relay's `/v1` HTTP surface.
// These mirror the serde DTOs in
// `crates/platform/kallip-lesche-common/src/{control,message,event}.rs` and the
// lesche's `crates/platform/kallip-lesche/src/routes/conversations.rs`. The
// lesche forwards `Envelope.ciphertext` and the byte fields below without
// interpreting them; every base64 string is STANDARD base64 (padded, +//),
// matching agora-common's bytes.rs.
//
// The chat wire types (`AuthoredEvent`, `SignalEvent`, `TagmaReply`,
// `FailoverChainExhausted`) live in `@kallipai/kallip-common` and are
// re-exported here, because the direct (offline) client shares them — the
// external chat-room vocabulary is transport-agnostic.

// Re-export the shared chat vocabulary (one source of truth in kallip-common).
export type {
  AuthoredEvent,
  FailoverChainExhausted,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallip-common";
import type { SignalEvent } from "@kallipai/kallip-common";

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
 * budget); `tagma_signal` carries per-event runtime signals (busy/idle presence,
 * turn terminals, errors). All non-envelope variants are plaintext and
 * user-scoped like presence — operator metadata, not conversation content. */
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
    }
  | {
      readonly type: "tagma_signal";
      readonly tagma_id: string;
      readonly event: SignalEvent;
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
