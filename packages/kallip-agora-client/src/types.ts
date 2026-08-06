// Response + error shapes for the agora `/v1` HTTP surface. These mirror the
// serde DTOs in `crates/platform/kallip-agora/src/routes/` (`auth.rs`, `tagmata.rs`,
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

/** `GET /v1/me/passkeys` — one of the caller's live passkeys. The agora's
 * `passkeys` table holds only live credentials (revoked history lives in a
 * separate audit table), so there is no status field. `label` is the user-
 * supplied device name ("" for the initial passkey until the user names it). */
export interface PasskeySummary {
  readonly id: string;
  readonly label: string;
  readonly created_at: string;
  /** RFC3339; seeded to the enrollment instant, updated on every sign-in. */
  readonly last_used_at: string;
}

/** Body the client sends to add-passkey `finish`. The label rides the finish
 * body (the begin txn never sees it). */
export interface AddPasskeyFinishRequest {
  readonly ceremony_id: string;
  readonly credential: RegisterPublicKeyCredentialJson;
  readonly label: string;
}

/** `PATCH /v1/me/passkeys/{id}` body. */
export interface RenamePasskeyRequest {
  readonly label: string;
}

/** `POST /v1/me/device-pairing` — a freshly minted, short-lived pairing code
 * (TTL is server-defined; see `expires_at`). `code` is the plaintext, returned
 * ONCE; only its hash is retained. */
export interface MintPairingCodeResponse {
  readonly code: string;
  readonly expires_at: string;
}

/** Body the new (unauthenticated) device sends to pair `begin`. */
export interface PairBeginRequest {
  readonly code: string;
}

/** Body the new device sends to pair `finish`. The label rides the finish body
 *  (the begin txn never sees it). */
export interface PairFinishRequest {
  readonly ceremony_id: string;
  readonly credential: RegisterPublicKeyCredentialJson;
  readonly label: string;
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

/** `GET /v1/users/{username}` -- a public user profile card. Minimal disclosure:
 * never `email`, `user_id`, or `passkey_count`. An unknown/disabled username and
 * a malformed input both 404 (existence-oracle; no shape leak). */
export interface PublicUserProfile {
  readonly username: string;
  readonly display_name: string | null;
  readonly created_at: string;
}

/** Lifecycle phase of a tagma. `pending` carries an unredeemed enrollment code;
 * `enrolled` has a tagma connected with a pinned device key. Revoked tagmas are
 * never listed. */
export type TagmaState = "pending" | "enrolled";

/**
 * `GET /v1/tagmata`. One tagma across its lifecycle. This is the registry
 * view only -- it carries NO liveness signal. Whether a tagma tunnel is
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

/** `GET /v1/tagmata/{id}` -- the tagma's pinned Ed25519 device key (TOFU). The
 * app verifies the lesche's key-exchange signature against it. `pinned_public_key`
 * is a 32-byte Ed25519 public key as standard base64; the caller passes this
 * string verbatim to `openRelayChannel` in `@kallipai/kallip-lesche-client`. */
export interface TagmaInfo {
  readonly tagma_id: string;
  readonly pinned_public_key: string;
}

/** `GET /v1/tagmata/{id}/profile` -- a public tagma profile card. Minimal
 * disclosure: never `pinned_public_key`, `owner_user_id`, or the enrolled/
 * revoked flags. Unknown/pending/revoked tagmas and a tagma whose owner is
 * disabled all 404 (existence-oracle; no state leak). */
export interface PublicTagmaProfile {
  readonly tagma_id: string;
  readonly label: string | null;
  readonly owner_username: string;
  readonly owner_display_name: string | null;
  readonly created_at: string;
}

// -- rooms ---------------------------------------------------------------
// Moved to @kallipai/kallip-lesche-client (the chat domain lives in lesche
// now). Import RoomView / RoomRosterView / RoomInviteView / CreateInvite* /
// AddTagmaRequest / RoomMemberProfile / ParticipantKind from there.

/**
 * Agora API error. Mirrors `kallip_common::protocol::ApiError`. This is a
 * distinct surface from `kallip-ui`'s tagma-transport `classifyError` -- the
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
