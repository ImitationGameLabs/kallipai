// Response + error shapes for the agora `/v1` HTTP surface. These mirror the
// serde DTOs in `crates/kallip-agora/src/routes/` (`auth.rs`, `tagmata.rs`,
// `admin.rs`). Timestamps are RFC3339 strings (time::OffsetDateTime serde)
// unless noted.

import type {
  PublicKeyCredentialJson,
  RegisterPublicKeyCredentialJson,
  ServerCreationOptions,
  ServerRequestOptions,
} from "./webauthn.ts";

/** `{ ceremony_id, options }` returned by register/login `begin`. */
export interface CeremonyBeginResponse<T> {
  readonly ceremony_id: string;
  readonly options: T;
}

export type RegisterBeginResponse =
  CeremonyBeginResponse<ServerCreationOptions>;
export type LoginBeginResponse = CeremonyBeginResponse<ServerRequestOptions>;

/** Bodies the client sends to register/login `finish`. */
export interface RegisterFinishRequest {
  readonly ceremony_id: string;
  readonly credential: RegisterPublicKeyCredentialJson;
}
export interface LoginFinishRequest {
  readonly ceremony_id: string;
  readonly credential: PublicKeyCredentialJson;
}

/** `{ user_id }` returned by register/login `finish`. */
export interface AuthFinishResponse {
  readonly user_id: string;
}

/** `GET /v1/me`. `display_name` is nullable (null when unset) -- the agora
 * returns `users.display_name` verbatim with no synthesis (see commit
 * 53aa563); presentation fallback belongs to the frontend. */
export interface MeResponse {
  readonly user_id: string;
  readonly username: string;
  readonly email: string;
  readonly display_name: string | null;
  readonly created_at: string;
  readonly passkey_count: number;
}

/** Lifecycle phase of a tagma. `pending` carries an unredeemed enrollment code;
 * `enrolled` has a herald connected with a pinned device key. Revoked tagmas are
 * never listed. */
export type TagmaState = "pending" | "enrolled";

/**
 * `GET /v1/tagmata`. One tagma across its lifecycle. This is the registry
 * view only -- it carries NO liveness signal. Whether a herald tunnel is
 * currently open arrives via the data plane: the lesche's `GET /v1/me/events`
 * SSE stream emits `tagma_online` / `tagma_offline` events (plus an initial
 * presence snapshot on connect). The pending-phase fields `code_masked` and
 * `expires_at` are present only while `state === "pending"` (the agora omits
 * them for enrolled rows). `code_masked` is the display-safe form
 * (`sk-enroll-abc***xyz`); the full plaintext is returned only once, on
 * {@link MintTagmaResponse.code}.
 */
export interface TagmaView {
  readonly tagma_id: string;
  readonly label: string | null;
  readonly state: TagmaState;
  readonly created_at: string;
  readonly code_masked?: string;
  readonly expires_at?: string;
}

/** `POST /v1/tagmata` (mint a pending tagma). `code` is the plaintext, returned
 * ONCE; only its hash is retained. `id` is the tagma id, stable across the enroll
 * transition. */
export interface MintTagmaResponse {
  readonly code: string;
  readonly id: string;
  readonly created_at: string;
  readonly expires_at: string;
}

/** `PATCH /v1/tagmata/{id}` body. `null` (or empty/whitespace) clears the label. */
export interface RenameTagmaRequest {
  readonly label: string | null;
}

// ---------------------------------------------------------------------------
// Chat data-plane (E2EE relay). Mirrors the serde DTOs in
// crates/kallip-agora-common/src/{control,message,event,herald}.rs and
// crates/kallip-agora/src/routes/{tagmata,conversations}.rs. The agora forwards
// `Envelope.ciphertext` and the byte fields below without interpreting them;
// every base64 string is STANDARD base64 (padded, +//), matching bytes.rs.
// ---------------------------------------------------------------------------

/** `GET /v1/tagmata/{id}` -- the tagma's pinned Ed25519 device key (TOFU). The
 * app verifies the key-exchange signature against it. `pinned_public_key` is a
 * 32-byte Ed25519 public key as standard base64. */
export interface TagmaInfo {
  readonly tagma_id: string;
  readonly pinned_public_key: string;
}

/** `POST /v1/conversations { tagma_id }` -- resolves the single conversation a
 * tagma owns with its operator (idempotent; id derived from the tagma). */
export interface CreateConversationResponse {
  readonly conversation_id: string;
}

/** Who sent an envelope. The agora is agent-free: an agent sender is attributed
 * only to its tagma. serde tag = `kind`, snake_case. */
export type Participant =
  | { readonly kind: "user"; readonly user_id: string }
  | { readonly kind: "agent"; readonly tagma_id: string };

/** The unit the agora forwards. `sequence_n` is per-conversation, per-sender,
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

/** App -> herald: one semantic op against the tagma, encrypted in an envelope.
 * serde tag = `op`, snake_case. `req_id` correlates the op with its TagmaReply. */
export type TagmaRequest =
  | {
      readonly op: "send_message";
      readonly req_id: number;
      readonly text: string;
    }
  | { readonly op: "interrupt"; readonly req_id: number };

/** Why a failover chain ran out. Mirrors `event.rs::FailoverChainExhaustion`
 * (serde `rename_all = "camelCase"`). */
export type FailoverChainExhaustion =
  | "noFailoverConfigured"
  | "allBackupsExhausted"
  | "allCandidatesUnbuildable"
  | "allCandidatesInfeasible";

/** An event the tagma emits to the app (the agent-free subset of the daemon's
 * event stream, mapped by the herald). serde tag = `type`, snake_case. There is
 * no streaming on this path: `assistant_content` / `finished` are each complete
 * messages. */
export type TagmaEvent =
  | { readonly type: "assistant_content"; readonly content: string }
  | { readonly type: "finished"; readonly content: string }
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

/** Herald -> app: either the result of a correlated op, or an unsolicited event
 * from the tagma's event pump. serde tag = `kind`, snake_case. */
export type TagmaReply =
  | {
      readonly kind: "message_accepted";
      readonly req_id: number;
      readonly queue_depth: number;
      readonly warning?: string;
    }
  | { readonly kind: "interrupted"; readonly req_id: number }
  | {
      readonly kind: "error";
      readonly req_id: number;
      readonly status: number;
      readonly message: string;
    }
  | { readonly kind: "event"; readonly event: TagmaEvent };

/** App -> herald (relayed by the agora): start a 1-RTT key exchange, carrying
 * the app's ephemeral X25519 public key (standard base64). */
export interface KeyExchangeInit {
  readonly ephemeral_public: string;
}

/** Herald -> app: the herald's ephemeral X25519 public key plus an Ed25519
 * signature over the kex transcript (standard base64). */
export interface KeyExchangeResponse {
  readonly ephemeral_public: string;
  readonly signature: string;
}

/** An event on the app's multiplexed SSE stream (`GET /v1/me/events`). serde tag
 * = `type`, snake_case. `envelope` plus `tagma_online` / `tagma_offline` are
 * emitted today (the presence pair is the sole liveness signal); `agent_state`
 * remains reserved. */
export type AgoraEvent =
  | { readonly type: "envelope"; readonly envelope: Envelope }
  | { readonly type: "tagma_online"; readonly tagma_id: string }
  | { readonly type: "tagma_offline"; readonly tagma_id: string }
  | {
      readonly type: "agent_state";
      readonly tagma_id: string;
      readonly agent_id: string;
      readonly state: string;
    };

/**
 * Agora API error. Mirrors `kallip_common::protocol::ApiError`. This is a
 * distinct surface from `kallip-ui`'s daemon-transport `classifyError` -- the
 * agora errors are rendered inline by the auth/dashboard pages, not through the
 * shared AppShell banner.
 */
export class AgoraApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "AgoraApiError";
  }
}
