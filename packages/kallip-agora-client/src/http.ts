// Browser client for the agora control-plane relay. The agora (default :7100)
// is the control plane: passkey ceremonies, `/me`, and the tagma lifecycle. It
// shares a session cookie cross-subdomain with the lesche (data plane), so every
// fetch carries `credentials: "include"` (the session cookie is the auth) and
// every non-GET carries the CSRF marker (`X-Requested-With: kallip`), which the
// agora's `csrf_guard` requires on cookie-bearing mutating requests. Non-2xx
// responses become `AgoraApiError` (`{ status, message }`). The data-plane
// client lives in `@kallipai/kallip-lesche-client`.

import { AgoraApiError } from "./types.ts";
import type {
  AddEmailRequest,
  AddPasskeyFinishRequest,
  AuthFinishResponse,
  EmailSummary,
  ExternalIdentitySummary,
  LoginBeginResponse,
  LoginFinishRequest,
  MeResponse,
  MintPairingCodeResponse,
  MintTagmaResponse,
  OAuthBeginResponse,
  OAuthFinishRequest,
  OAuthNeedsUsernameResponse,
  OAuthSignupCompleteRequest,
  PairBeginRequest,
  PairFinishRequest,
  PasskeySummary,
  ProviderInfo,
  PublicTagmaProfile,
  PublicUserProfile,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenamePasskeyRequest,
  RenameTagmaRequest,
  TagmaInfo,
  TagmaView,
  VerifyEmailRequest,
} from "./types.ts";

/** CSRF marker the agora's `csrf_guard` checks (see `session.rs:21-24`). */
export const CSRF_HEADER = "X-Requested-With";
export const CSRF_HEADER_VALUE = "kallip";

/** Request bodies for the ceremony begins. Mirrors the agora DTOs in
 * `crates/platform/kallip-agora/src/routes/auth.rs` (`RegisterBeginRequest`,
 * `LoginBeginRequest`): the username is the login id (login resolves by
 * username); email is no longer collected at registration -- it is an optional
 * contact channel the user links later in settings. */
export interface RegisterBeginRequest {
  readonly username: string;
  readonly display_name?: string;
}
export interface LoginBeginRequest {
  readonly username: string;
}

/**
 * Shared base for the agora browser client: a base URL + the JSON/CSRF fetch
 * helper. The session cookie (`credentials: "include"`) is the auth; the
 * `X-Requested-With` CSRF marker is required on cookie-bearing mutating
 * requests. Non-2xx responses become `AgoraApiError` (`{ status, message }`).
 *
 * Internal to this package: the agora surface has a single client, so the base
 * is not re-exported.
 */
abstract class BaseClient {
  constructor(protected readonly baseUrl: string) {}

  /** JSON fetch with the CSRF marker on non-GETs; `AgoraApiError` on non-2xx. */
  protected async json<T>(
    path: string,
    method: string,
    body?: unknown,
  ): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    // Send the CSRF marker on every non-GET unconditionally: it is required on
    // cookie-bearing mutating requests and harmless otherwise.
    const isStateChanging = method !== "GET";
    if (isStateChanging) {
      headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
      if (body !== undefined) headers["content-type"] = "application/json";
    }
    const resp = await fetch(this.baseUrl + path, {
      method,
      headers,
      credentials: "include",
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    if (!resp.ok) throw await agoraError(resp);
    // 204 No Content (revoke) -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return (await resp.json()) as T;
  }
}

/**
 * Control-plane client (the agora service, default :7100): passkey ceremonies,
 * `/me`, and the tagma lifecycle. Also exposes `getTagma` — the pinned device
 * key is TOFU from the control plane, even though the key exchange itself runs
 * on the lesche (see `@kallipai/kallip-lesche-client`).
 */
export class AgoraClient extends BaseClient {
  // -- auth ceremonies ------------------------------------------------------

  registerBegin(body: RegisterBeginRequest): Promise<RegisterBeginResponse> {
    return this.json("/v1/auth/register/begin", "POST", body);
  }

  registerFinish(body: RegisterFinishRequest): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/register/finish", "POST", body);
  }

  loginBegin(body: LoginBeginRequest): Promise<LoginBeginResponse> {
    return this.json("/v1/auth/login/begin", "POST", body);
  }

  loginFinish(body: LoginFinishRequest): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/login/finish", "POST", body);
  }

  /** `POST /v1/auth/login/discoverable/begin` — start a usernameless login. No
   * identifier: the authenticator resolves the user at finish via the
   * assertion's userHandle. The server returns conditional-mediation options
   * with an empty allowList. */
  loginDiscoverableBegin(): Promise<LoginBeginResponse> {
    return this.json("/v1/auth/login/discoverable/begin", "POST", undefined);
  }

  /** `POST /v1/auth/login/discoverable/finish` — verify the discoverable
   * assertion (the credential carries the userHandle the server identifies the
   * account by). */
  loginDiscoverableFinish(
    body: LoginFinishRequest,
  ): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/login/discoverable/finish", "POST", body);
  }

  logout(): Promise<void> {
    return this.json("/v1/auth/logout", "POST", undefined);
  }

  // -- oauth ----------------------------------------------------------------

  /** `GET /v1/auth/oauth/providers` — which OAuth providers this deploy has
   * configured (unauthenticated; for rendering the login/settings buttons). */
  listOAuthProviders(): Promise<ProviderInfo[]> {
    return this.json("/v1/auth/oauth/providers", "GET");
  }

  /** `POST /v1/auth/oauth/{provider}/begin` — start a signin ceremony
   * (anonymous). The SPA navigates to the returned `authorize_url`; the
   * provider redirects back to the SPA callback with `code`+`state`. */
  oauthSignInBegin(
    provider: string,
    body: { return_path?: string },
  ): Promise<OAuthBeginResponse> {
    return this.json(
      `/v1/auth/oauth/${encodeURIComponent(provider)}/begin`,
      "POST",
      body,
    );
  }

  /** `POST /v1/me/oauth/{provider}/begin` — start a link ceremony (cookie-
   * authed, step-up gated). Binds the next provider identity to the signed-in
   * account. */
  oauthLinkBegin(
    provider: string,
    body: { return_path?: string },
  ): Promise<OAuthBeginResponse> {
    return this.json(
      `/v1/me/oauth/${encodeURIComponent(provider)}/begin`,
      "POST",
      body,
    );
  }

  /** `POST /v1/auth/oauth/{provider}/finish` — complete the OAuth ceremony. The
   * same endpoint serves BOTH actions (the opaque `state` selects which): a
   * signin returns 200/201 with `{user_id, return_path?}` (and sets the session
   * cookie); a link returns 204 with no body (the caller is already signed in);
   * an UNLINKED signin returns 202 `{kind:"needs-username",...}` holding the
   * claim for a username step (no cookie). Resolves to `undefined` on the 204
   * link path, else the signin or needs-username body. */
  oauthFinish(
    provider: string,
    body: OAuthFinishRequest,
  ): Promise<AuthFinishResponse | OAuthNeedsUsernameResponse | undefined> {
    return this.json(
      `/v1/auth/oauth/${encodeURIComponent(provider)}/finish`,
      "POST",
      body,
    );
  }

  /** `POST /v1/auth/oauth/signup/complete` — finish a held OAuth signup by
   * submitting the chosen username against the single-use token from a 202
   * needs-username finish. Creates the account, binds the identity, and sets
   * the session cookie; returns `{user_id, return_path?}`. */
  oauthSignupComplete(
    body: OAuthSignupCompleteRequest,
  ): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/oauth/signup/complete", "POST", body);
  }

  /** The signed-in user's linked OAuth identities. Fetches `GET /v1/me` and
   * projects `external_identities` (no dedicated list route). */
  listExternalIdentities(): Promise<readonly ExternalIdentitySummary[]> {
    return this.json<MeResponse>("/v1/me", "GET").then((me) =>
      me.external_identities
    );
  }

  /** `DELETE /v1/me/external-identities/{id}` — unlink an identity (hard-delete;
   * 409 if it is the account's last sign-in method). */
  unlinkExternalIdentity(id: string): Promise<void> {
    return this.json(
      `/v1/me/external-identities/${encodeURIComponent(id)}`,
      "DELETE",
      undefined,
    );
  }

  // -- profile --------------------------------------------------------------

  me(): Promise<MeResponse> {
    return this.json("/v1/me", "GET");
  }

  // -- emails (self-service contact-channel management) ---------------------

  /** `GET /v1/me/emails` — the caller's linked addresses (oldest first). */
  listEmails(): Promise<EmailSummary[]> {
    return this.json("/v1/me/emails", "GET");
  }

  /** `POST /v1/me/emails` — link a new address (starts unverified; a
   * verification link is sent out-of-band). */
  addEmail(body: AddEmailRequest): Promise<EmailSummary> {
    return this.json("/v1/me/emails", "POST", body);
  }

  /** `POST /v1/me/emails/verify` — consume a verification token, marking the
   * address verified. */
  verifyEmail(body: VerifyEmailRequest): Promise<EmailSummary> {
    return this.json("/v1/me/emails/verify", "POST", body);
  }

  /** `POST /v1/me/emails/{id}` — promote a verified address to primary. */
  makeEmailPrimary(id: string): Promise<EmailSummary> {
    return this.json(`/v1/me/emails/${encodeURIComponent(id)}`, "POST", {});
  }

  /** `DELETE /v1/me/emails/{id}` — unlink an address. Returns the remaining
   * addresses (a different one is promoted to primary if the primary was
   * removed). */
  removeEmail(id: string): Promise<EmailSummary[]> {
    return this.json(`/v1/me/emails/${encodeURIComponent(id)}`, "DELETE");
  }

  // -- passkeys (self-service management of the caller's own devices) -------

  /** `GET /v1/me/passkeys` — the caller's live passkeys (oldest first). */
  listPasskeys(): Promise<PasskeySummary[]> {
    return this.json("/v1/me/passkeys", "GET");
  }

  /** `POST /v1/me/passkeys/register/begin` — start binding ANOTHER passkey to
   * the signed-in account. Gated by a one-shot step-up; returns 403
   * `reauth-required` if the session's freshness is stale/consumed (run
   * `loginWithPasskey` then retry). With `discoverable: true` the ceremony
   * enrolls a resident (passwordless) credential. */
  addPasskeyBegin(
    opts: { discoverable?: boolean } = {},
  ): Promise<RegisterBeginResponse> {
    const query = opts.discoverable ? "?discoverable=true" : "";
    return this.json(`/v1/me/passkeys/register/begin${query}`, "POST", {});
  }

  /** `POST /v1/me/passkeys/register/finish` — verify + bind the new passkey. */
  addPasskeyFinish(body: AddPasskeyFinishRequest): Promise<PasskeySummary> {
    return this.json("/v1/me/passkeys/register/finish", "POST", body);
  }

  /** `PATCH /v1/me/passkeys/{id}` — rename (the device label). */
  renamePasskey(id: string, label: string): Promise<PasskeySummary> {
    const body: RenamePasskeyRequest = { label };
    return this.json(
      `/v1/me/passkeys/${encodeURIComponent(id)}`,
      "PATCH",
      body,
    );
  }

  /** `DELETE /v1/me/passkeys/{id}` — revoke (hard-delete + audit row). The last
   * live passkey cannot be revoked (409). Returns on 204. */
  revokePasskey(id: string): Promise<void> {
    return this.json(
      `/v1/me/passkeys/${encodeURIComponent(id)}`,
      "DELETE",
      undefined,
    );
  }

  // -- device pairing (cross-device enrollment via a short-lived code) --------

  /** `POST /v1/me/device-pairing` (session-authed + step-up) — mint a
   * short-lived pairing code shown on this device for a new device to redeem.
   * Returns 403 `reauth-required` if the session's step-up is stale (run
   * `loginWithPasskey` then retry). */
  mintPairingCode(): Promise<MintPairingCodeResponse> {
    return this.json("/v1/me/device-pairing", "POST", {});
  }

  /** `POST /v1/auth/device-pairing/begin` (unauthenticated, rate-limited) —
   * start enrolling a LOCAL passkey on this new device onto an existing account
   * referenced by the code. */
  pairBegin(body: PairBeginRequest): Promise<RegisterBeginResponse> {
    return this.json("/v1/auth/device-pairing/begin", "POST", body);
  }

  /** `POST /v1/auth/device-pairing/finish` (unauthenticated) — verify + bind
   * the new passkey; mints a session for this device. */
  pairFinish(body: PairFinishRequest): Promise<AuthFinishResponse> {
    return this.json("/v1/auth/device-pairing/finish", "POST", body);
  }

  // -- tagmata (unified pending + enrolled lifecycle) -----------------------

  /** `POST /v1/tagmata` — mint a pending tagma (an enrollment code). The
   * plaintext `code` is returned once. */
  mintTagma(): Promise<MintTagmaResponse> {
    return this.json("/v1/tagmata", "POST", {});
  }

  /** `GET /v1/tagmata` — the caller's tagmata (pending + enrolled, not revoked),
   * newest first. Registry view only; liveness is NOT included (it arrives via
   * the lesche's `meEvents`). */
  listTagmata(): Promise<TagmaView[]> {
    return this.json("/v1/tagmata", "GET");
  }

  /** `PATCH /v1/tagmata/{id}` — set or clear the label (pending or enrolled).
   * Returns on 204. */
  renameTagma(id: string, label: string | null): Promise<void> {
    const body: RenameTagmaRequest = { label };
    return this.json(`/v1/tagmata/${encodeURIComponent(id)}`, "PATCH", body);
  }

  /** `DELETE /v1/tagmata/{id}` — revoke (pending or enrolled). For an enrolled
   * tagma the agora cuts the tagma off on its next request. Returns on 204. */
  revokeTagma(id: string): Promise<void> {
    return this.json(
      `/v1/tagmata/${encodeURIComponent(id)}`,
      "DELETE",
      undefined,
    );
  }

  /** `GET /v1/tagmata/{id}` — the tagma's pinned Ed25519 device key (TOFU). The
   * app verifies the lesche's key-exchange signature against it. */
  getTagma(id: string): Promise<TagmaInfo> {
    return this.json(`/v1/tagmata/${encodeURIComponent(id)}`, "GET");
  }

  /** `GET /v1/users/{username}` — a public user profile card. Public + per-IP
   * rate-limited; an unknown/disabled/malformed username 404s. */
  getUserProfile(username: string): Promise<PublicUserProfile> {
    return this.json(`/v1/users/${encodeURIComponent(username)}`, "GET");
  }

  /** `GET /v1/tagmata/{id}/profile` — a public tagma profile card. Public +
   * per-IP rate-limited; an unknown/pending/revoked tagma (or one whose owner
   * is disabled) 404s. */
  getTagmaProfile(id: string): Promise<PublicTagmaProfile> {
    return this.json(`/v1/tagmata/${encodeURIComponent(id)}/profile`, "GET");
  }
}

/** Build an `AgoraApiError` from a non-2xx response: the agora's `ApiError` body
 * is `{ status, message }`; fall back to `statusText` for a non-JSON body (e.g. a
 * 403 from the CSRF guard). */
async function agoraError(resp: Response): Promise<AgoraApiError> {
  let message = resp.statusText;
  try {
    const errorBody = (await resp.json()) as { message?: string };
    if (errorBody.message) message = errorBody.message;
  } catch {
    // Non-JSON error body; keep statusText.
  }
  return new AgoraApiError(resp.status, message);
}
