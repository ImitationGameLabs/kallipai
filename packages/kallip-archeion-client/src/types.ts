// Response + error shapes for the archeion `/v1` HTTP surface. These mirror the
// serde DTOs in `crates/platform/kallip-archeion/src/routes/` (`auth.rs`, `tagmata.rs`,
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
  /** OAuth signin only: the sanitized return path to resume to (omitted when
   * the server has none). The passkey ceremonies do not set it. */
  readonly return_path?: string;
}

/** The four outcomes of the signup availability probe (`GET
 * /auth/username-availability`). Always delivered as a 200 body status:
 * every outcome, invalid shape included, is an answer, not an error path. */
export type UsernameAvailabilityStatus =
  | "available"
  | "taken"
  | "reserved"
  | "invalid";

export interface UsernameAvailabilityResponse {
  /** The canonical (normalized) handle for a valid shape; the raw query
   * echoed back when the shape is invalid, so the caller can match the
   * response to the probe that issued it. */
  readonly username: string;
  readonly status: UsernameAvailabilityStatus;
}

/** `GET /auth/oauth/providers` — one enabled OAuth provider, for rendering
 * the login/settings buttons. */
export interface ProviderInfo {
  /** Stable provider id: `"github"` | `"google"`. */
  readonly id: string;
  /** Human label, e.g. `"GitHub"`. */
  readonly label: string;
}

/** A linked OAuth identity, as surfaced by `/me`. */
export interface ExternalIdentitySummary {
  readonly id: string;
  /** `"github"` | `"google"`. */
  readonly provider: string;
  readonly display_name: string | null;
  readonly created_at: string;
  readonly last_used_at: string | null;
}

/** `POST .../oauth/{provider}/begin` — the provider authorize URL the SPA
 * navigates to. */
export interface OAuthBeginResponse {
  readonly authorize_url: string;
}

/** `POST /auth/oauth/{provider}/finish` body: the provider's `code` + the
 * single-use `state` token it echoed back. */
export interface OAuthFinishRequest {
  readonly state: string;
  readonly code: string;
}

/** `POST .../oauth/{provider}/finish` 202 response: the OAuth identity was not
 * linked, so the resolved claim is held server-side and the SPA must collect a
 * chosen username, then submit it (with this single-use token) at
 * `oauthSignupComplete`. No session is established yet. */
export interface OAuthNeedsUsernameResponse {
  readonly kind: "needs-username";
  readonly signup_token: string;
  readonly provider: string;
}

/** `POST /auth/oauth/signup/complete` body: the single-use token from a
 * 202 needs-username finish, plus the user-chosen username. Account creation +
 * session minting happen here. */
export interface OAuthSignupCompleteRequest {
  readonly signup_token: string;
  readonly username: string;
}

/** `GET /me/passkeys` — one of the caller's live passkeys. The archeion's
 * `passkeys` table holds only live credentials (revoked history lives in a
 * separate audit table), so there is no status field. `label` is the user-
 * supplied device name ("" for the initial passkey until the user names it). */
export interface PasskeySummary {
  readonly id: string;
  readonly label: string;
  readonly created_at: string;
  /** RFC3339; seeded to the enrollment instant, updated on every sign-in. */
  readonly last_used_at: string;
  /** Whether this credential was enrolled via the discoverable (resident-key)
   * flow -- gates the "passwordless sign-in" affordance. */
  readonly discoverable: boolean;
}

/** Body the client sends to add-passkey `finish`. The label rides the finish
 * body (the begin txn never sees it). */
export interface AddPasskeyFinishRequest {
  readonly ceremony_id: string;
  readonly credential: RegisterPublicKeyCredentialJson;
  readonly label: string;
}

/** `PATCH /me/passkeys/{id}` body. */
export interface RenamePasskeyRequest {
  readonly label: string;
}

/** `POST /me/device-pairing` — a freshly minted, short-lived pairing code
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

/** One linked email address, as returned by `/me` and `/me/emails`.
 * `verified_at` is null until the user completes the verification flow. */
export interface EmailSummary {
  readonly id: string;
  readonly address: string;
  readonly is_primary: boolean;
  readonly verified_at: string | null;
}

/** `GET /me`. Email is an optional contact channel, decoupled from login
 * (which resolves by username): `emails` is empty until the user links one in
 * settings, and `primary_email` is null then. `display_name` is nullable (null
 * when unset) -- the archeion returns `users.display_name` verbatim with no
 * synthesis; presentation fallback belongs to the frontend. */
export interface MeResponse {
  readonly user_id: string;
  readonly username: string;
  /** All linked addresses; empty until the user adds one. */
  readonly emails: readonly EmailSummary[];
  /** Primary contact address, or null when the user has no email. */
  readonly primary_email: string | null;
  /** Linked OAuth identities; empty for a passkey-only account. */
  readonly external_identities: readonly ExternalIdentitySummary[];
  readonly display_name: string | null;
  readonly created_at: string;
  readonly passkey_count: number;
}

/** `POST /me/emails` body. The address starts unverified; a verification
 * link is delivered out-of-band. */
export interface AddEmailRequest {
  readonly address: string;
}

/** `POST /me/emails/verify` body. `token` is the single-use secret from the
 * verification link. */
export interface VerifyEmailRequest {
  readonly token: string;
}

/** Storage mode of a user-provider key. `plaintext` stores the raw key
 * readable by any session of the account; `encrypted` stores an opaque blob
 * sealed by a device-held vault key that the archeion can neither read nor
 * decrypt (the service is blind to encrypted rows by design). */
export type ProviderKeyMode = "plaintext" | "encrypted";

/** One entry of the caller's provider vault (`GET /me/providers`).
 * `name` is unique per account; collisions on create/replace are 409.
 * `key_material` returns as stored -- the vault never transforms it. */
export interface ProviderSummary {
  readonly id: string;
  readonly name: string;
  readonly provider: string;
  readonly base_url: string | null;
  /** Raw key when `mode` is plaintext, client-encrypted blob otherwise. */
  readonly key_material: string;
  readonly mode: ProviderKeyMode;
  readonly created_at: string;
  readonly updated_at: string;
}

/** Create/replace body for `/me/providers`. Replace sends EVERY field
 * (metadata edit, key rotation, and mode flip share one explicit shape). */
export interface ProviderRequest {
  readonly name: string;
  readonly provider: string;
  readonly base_url?: string | null;
  readonly key_material: string;
  readonly mode: ProviderKeyMode;
}

/** `GET /users/{username}` -- a public user profile card. Minimal disclosure:
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
 * `GET /tagmata`. One tagma across its lifecycle. This is the registry
 * view only -- it carries NO liveness signal. Whether a tagma tunnel is
 * currently open arrives via the data plane: the lesche's `GET /me/events`
 * SSE stream emits `tagma_online` / `tagma_offline` events (plus an initial
 * presence snapshot on connect). The pending-phase fields `code_masked` and
 * `expires_at` are present only while `state === "pending"` (the archeion omits
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

/** `POST /tagmata` (mint a pending tagma). `code` is the plaintext, returned
 * ONCE; only its hash is retained. `id` is the tagma id, stable across the enroll
 * transition. */
export interface MintTagmaResponse {
  readonly code: string;
  readonly id: string;
  readonly created_at: string;
  readonly expires_at: string;
}

/** `PATCH /tagmata/{id}` body. `null` (or empty/whitespace) clears the label. */
export interface RenameTagmaRequest {
  readonly label: string | null;
}

/** `GET /tagmata/{id}` -- the tagma's pinned Ed25519 device key (TOFU). The
 * app verifies the lesche's key-exchange signature against it. `pinned_public_key`
 * is a 32-byte Ed25519 public key as standard base64; the caller passes this
 * string verbatim to `openRelayChannel` in `@kallipai/kallip-lesche-client`. */
export interface TagmaInfo {
  readonly tagma_id: string;
  readonly pinned_public_key: string;
}

/** `GET /tagmata/{id}/profile` -- a public tagma profile card. Minimal
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
 * Archeion API error. Mirrors `kallip_common::protocol::ApiError`. This is a
 * distinct surface from `kallip-ui`'s tagma-transport `classifyError` -- the
 * archeion errors are rendered inline by the auth/dashboard pages, not through the
 * shared AppShell banner.
 */
export class ArcheionApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ArcheionApiError";
  }
}
