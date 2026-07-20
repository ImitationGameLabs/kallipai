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
 * `GET /v1/tagmata`. One tagma across its lifecycle. `online` is the sole
 * liveness signal (an in-memory presence map); pending tagmas are always
 * `online=false`. The pending-phase fields `code_masked` and `expires_at` are
 * present only while `state === "pending"` (the agora omits them for enrolled
 * rows). `code_masked` is the display-safe form (`sk-enroll-abc***xyz`); the
 * full plaintext is returned only once, on {@link MintTagmaResponse.code}.
 */
export interface TagmaView {
  readonly tagma_id: string;
  readonly label: string | null;
  readonly state: TagmaState;
  readonly created_at: string;
  readonly online: boolean;
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
