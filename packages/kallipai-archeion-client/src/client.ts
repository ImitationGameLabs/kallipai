// The archeion control plane's HTTP verbs, as effects, plus the promise face
// class the UI awaits. One verb per route, plus one projection over `/me`:
// passkey ceremonies, `/me`, the OAuth ceremonies, emails, passkeys, device
// pairing, and the tagma lifecycle (mint/rename/revoke plus the pinned
// device-key fetch). The exchange itself lives in ./http.ts.

import { runClientEffect } from "@kallipai/kallipai-common";
import { Effect } from "effect";

import { type ArcheionFailure, archeionRequest } from "./http.ts";

import type {
  AddEmailRequest,
  AddPasskeyFinishRequest,
  AuthFinishResponse,
  CreateTokenRequest,
  EmailSummary,
  ExternalIdentitySummary,
  IssuedToken,
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
  TagmaCredentialUpdateRequest,
  TagmaCredentialView,
  TagmaInfo,
  TagmaView,
  TokenView,
  UpdateTokenRequest,
  UsernameAvailabilityResponse,
  VerifyEmailRequest,
} from "./types.ts";

/** Request bodies for the ceremony begins. Mirrors the archeion DTOs in
 * `crates/platform/kallipai-archeion/src/routes/auth/register.rs`
 * (`RegisterBeginRequest`) and
 * `crates/platform/kallipai-archeion/src/routes/auth/login.rs`
 * (`LoginBeginRequest`): the username is the login id (login resolves by
 * username); email is no longer collected at registration -- it is an optional
 * contact channel the user links later in settings. */
export interface RegisterBeginRequest {
  readonly username: string;
  readonly display_name?: string;
}
export interface LoginBeginRequest {
  readonly username: string;
}
// -- auth ceremonies ------------------------------------------------------

/** `POST /auth/register/begin` -- reserve a registration ceremony and answer
 * the WebAuthn creation options. */
export function registerBegin(
  base: string,
  body: RegisterBeginRequest,
): Effect.Effect<RegisterBeginResponse, ArcheionFailure> {
  return archeionRequest<RegisterBeginResponse>(base, "/auth/register/begin", {
    method: "POST",
    body: body,
  });
}

/** `POST /auth/register/finish` -- verify the new credential and mint the
 * session. */
export function registerFinish(
  base: string,
  body: RegisterFinishRequest,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(base, "/auth/register/finish", {
    method: "POST",
    body: body,
  });
}

/** `POST /auth/login/begin` -- start a passkey login for one username. */
export function loginBegin(
  base: string,
  body: LoginBeginRequest,
): Effect.Effect<LoginBeginResponse, ArcheionFailure> {
  return archeionRequest<LoginBeginResponse>(base, "/auth/login/begin", {
    method: "POST",
    body: body,
  });
}

/** `POST /auth/login/finish` -- verify the assertion and mint the session. */
export function loginFinish(
  base: string,
  body: LoginFinishRequest,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(base, "/auth/login/finish", {
    method: "POST",
    body: body,
  });
}

/** `POST /auth/admin-login` — exchange the operator's `sk-admin-` key
 * for a User session on the fixed local account. The route is mounted only
 * where the deployment's boot flag set it; elsewhere this 404s. */
export function adminLogin(
  base: string,
  key: string,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(base, "/auth/admin-login", {
    method: "POST",
    headers: { authorization: `Bearer ${key}` },
  });
}

/** `GET /auth/username-availability` — the signup form's availability
 * probe. Always a 200 whose body carries the status (available / taken /
 * reserved / invalid); the server per-IP rate-limits this oracle. */
export function usernameAvailability(
  base: string,
  username: string,
): Effect.Effect<UsernameAvailabilityResponse, ArcheionFailure> {
  return archeionRequest<UsernameAvailabilityResponse>(
    base,
    `/auth/username-availability?username=${encodeURIComponent(username)}`,
    {
      method: "GET",
    },
  );
}

/** `POST /auth/login/discoverable/begin` — start a usernameless login. No
 * identifier: the authenticator resolves the user at finish via the
 * assertion's userHandle. The server returns conditional-mediation options
 * with an empty allowList. */
export function loginDiscoverableBegin(
  base: string,
): Effect.Effect<LoginBeginResponse, ArcheionFailure> {
  return archeionRequest<LoginBeginResponse>(
    base,
    "/auth/login/discoverable/begin",
    {
      method: "POST",
    },
  );
}

/** `POST /auth/login/discoverable/finish` — verify the discoverable
 * assertion (the credential carries the userHandle the server identifies the
 * account by). */
export function loginDiscoverableFinish(
  base: string,
  body: LoginFinishRequest,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(
    base,
    "/auth/login/discoverable/finish",
    {
      method: "POST",
      body: body,
    },
  );
}

/** `POST /auth/logout` -- drop the session; 204 with no body. */
export function logout(base: string): Effect.Effect<void, ArcheionFailure> {
  return archeionRequest<void>(base, "/auth/logout", {
    method: "POST",
  });
}

// -- oauth ----------------------------------------------------------------

/** `GET /auth/oauth/providers` — which OAuth providers this deploy has
 * configured (unauthenticated; for rendering the login/settings buttons). */
export function listOAuthProviders(
  base: string,
): Effect.Effect<ProviderInfo[], ArcheionFailure> {
  return archeionRequest<ProviderInfo[]>(base, "/auth/oauth/providers", {
    method: "GET",
  });
}

/** `POST /auth/oauth/{provider}/begin` — start a signin ceremony
 * (anonymous). The SPA navigates to the returned `authorize_url`; the
 * provider redirects back to the SPA callback with `code`+`state`. */
export function oauthSignInBegin(
  base: string,
  provider: string,
  body: { return_path?: string },
): Effect.Effect<OAuthBeginResponse, ArcheionFailure> {
  return archeionRequest<OAuthBeginResponse>(
    base,
    `/auth/oauth/${encodeURIComponent(provider)}/begin`,
    {
      method: "POST",
      body: body,
    },
  );
}

/** `POST /me/oauth/{provider}/begin` — start a link ceremony (cookie-
 * authed, step-up gated). Binds the next provider identity to the signed-in
 * account. */
export function oauthLinkBegin(
  base: string,
  provider: string,
  body: { return_path?: string },
): Effect.Effect<OAuthBeginResponse, ArcheionFailure> {
  return archeionRequest<OAuthBeginResponse>(
    base,
    `/me/oauth/${encodeURIComponent(provider)}/begin`,
    {
      method: "POST",
      body: body,
    },
  );
}

/** `POST /auth/oauth/{provider}/finish` — complete the OAuth ceremony. The
 * same endpoint serves BOTH actions (the opaque `state` selects which): a
 * signin returns 200/201 with `{user_id, return_path?}` (and sets the session
 * cookie); a link returns 204 with no body (the caller is already signed in);
 * an UNLINKED signin returns 202 `{kind:"needs-username",...}` holding the
 * claim for a username step (no cookie). Resolves to `undefined` on the 204
 * link path, else the signin or needs-username body. */
export function oauthFinish(
  base: string,
  provider: string,
  body: OAuthFinishRequest,
): Effect.Effect<
  AuthFinishResponse | OAuthNeedsUsernameResponse | undefined,
  ArcheionFailure
> {
  return archeionRequest<
    AuthFinishResponse | OAuthNeedsUsernameResponse | undefined
  >(base, `/auth/oauth/${encodeURIComponent(provider)}/finish`, {
    method: "POST",
    body: body,
  });
}

/** `POST /auth/oauth/signup/complete` — finish a held OAuth signup by
 * submitting the chosen username against the single-use token from a 202
 * needs-username finish. Creates the account, binds the identity, and sets
 * the session cookie; returns `{user_id, return_path?}`. */
export function oauthSignupComplete(
  base: string,
  body: OAuthSignupCompleteRequest,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(
    base,
    "/auth/oauth/signup/complete",
    {
      method: "POST",
      body: body,
    },
  );
}

/** The signed-in user's linked OAuth identities. Fetches `GET /me` and
 * projects `external_identities` (no dedicated list route). */
export function listExternalIdentities(
  base: string,
): Effect.Effect<readonly ExternalIdentitySummary[], ArcheionFailure> {
  return me(base).pipe(Effect.map((profile) => profile.external_identities));
}

/** `DELETE /me/external-identities/{id}` — unlink an identity (hard-delete;
 * 409 if it is the account's last sign-in method). */
export function unlinkExternalIdentity(
  base: string,
  id: string,
): Effect.Effect<void, ArcheionFailure> {
  return archeionRequest<void>(
    base,
    `/me/external-identities/${encodeURIComponent(id)}`,
    {
      method: "DELETE",
    },
  );
}

// -- profile --------------------------------------------------------------

/** `GET /me` -- the signed-in user's profile. */
export function me(base: string): Effect.Effect<MeResponse, ArcheionFailure> {
  return archeionRequest<MeResponse>(base, "/me", {
    method: "GET",
  });
}

// -- emails (self-service contact-channel management) ---------------------

/** `GET /me/emails` — the caller's linked addresses (oldest first). */
export function listEmails(
  base: string,
): Effect.Effect<EmailSummary[], ArcheionFailure> {
  return archeionRequest<EmailSummary[]>(base, "/me/emails", {
    method: "GET",
  });
}

/** `POST /me/emails` — link a new address (starts unverified; a
 * verification link is sent out-of-band). */
export function addEmail(
  base: string,
  body: AddEmailRequest,
): Effect.Effect<EmailSummary, ArcheionFailure> {
  return archeionRequest<EmailSummary>(base, "/me/emails", {
    method: "POST",
    body: body,
  });
}

/** `POST /me/emails/verify` — consume a verification token, marking the
 * address verified. */
export function verifyEmail(
  base: string,
  body: VerifyEmailRequest,
): Effect.Effect<EmailSummary, ArcheionFailure> {
  return archeionRequest<EmailSummary>(base, "/me/emails/verify", {
    method: "POST",
    body: body,
  });
}

/** `POST /me/emails/{id}` — promote a verified address to primary. */
export function makeEmailPrimary(
  base: string,
  id: string,
): Effect.Effect<EmailSummary, ArcheionFailure> {
  return archeionRequest<EmailSummary>(
    base,
    `/me/emails/${encodeURIComponent(id)}`,
    {
      method: "POST",
      body: {},
    },
  );
}

/** `DELETE /me/emails/{id}` — unlink an address. Returns the remaining
 * addresses (a different one is promoted to primary if the primary was
 * removed). */
export function removeEmail(
  base: string,
  id: string,
): Effect.Effect<EmailSummary[], ArcheionFailure> {
  return archeionRequest<EmailSummary[]>(
    base,
    `/me/emails/${encodeURIComponent(id)}`,
    {
      method: "DELETE",
    },
  );
}

// -- passkeys (self-service management of the caller's own devices) -------

/** `GET /me/passkeys` — the caller's live passkeys (oldest first). */
export function listPasskeys(
  base: string,
): Effect.Effect<PasskeySummary[], ArcheionFailure> {
  return archeionRequest<PasskeySummary[]>(base, "/me/passkeys", {
    method: "GET",
  });
}

/** `POST /me/passkeys/register/begin` — start binding ANOTHER passkey to
 * the signed-in account. Gated by a one-shot step-up; returns 403
 * `reauth-required` if the session's freshness is stale/consumed (run
 * `loginWithPasskey` then retry). With `discoverable: true` the ceremony
 * enrolls a resident (passwordless) credential. */
export function addPasskeyBegin(
  base: string,
  opts: { discoverable?: boolean } = {},
): Effect.Effect<RegisterBeginResponse, ArcheionFailure> {
  const query = opts.discoverable ? "?discoverable=true" : "";
  return archeionRequest<RegisterBeginResponse>(
    base,
    `/me/passkeys/register/begin${query}`,
    {
      method: "POST",
      body: {},
    },
  );
}

/** `POST /me/passkeys/register/finish` — verify + bind the new passkey. */
export function addPasskeyFinish(
  base: string,
  body: AddPasskeyFinishRequest,
): Effect.Effect<PasskeySummary, ArcheionFailure> {
  return archeionRequest<PasskeySummary>(base, "/me/passkeys/register/finish", {
    method: "POST",
    body: body,
  });
}

/** `PATCH /me/passkeys/{id}` — rename (the device label). */
export function renamePasskey(
  base: string,
  id: string,
  label: string,
): Effect.Effect<PasskeySummary, ArcheionFailure> {
  const body: RenamePasskeyRequest = { label };
  return archeionRequest<PasskeySummary>(
    base,
    `/me/passkeys/${encodeURIComponent(id)}`,
    {
      method: "PATCH",
      body: body,
    },
  );
}

/** `DELETE /me/passkeys/{id}` — revoke (hard-delete + audit row). The last
 * live passkey cannot be revoked (409). Returns on 204. */
export function revokePasskey(
  base: string,
  id: string,
): Effect.Effect<void, ArcheionFailure> {
  return archeionRequest<void>(base, `/me/passkeys/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

// -- device pairing (cross-device enrollment via a short-lived code) --------

/** `POST /me/device-pairing` (session-authed + step-up) — mint a
 * short-lived pairing code shown on this device for a new device to redeem.
 * Returns 403 `reauth-required` if the session's step-up is stale (run
 * `loginWithPasskey` then retry). */
export function mintPairingCode(
  base: string,
): Effect.Effect<MintPairingCodeResponse, ArcheionFailure> {
  return archeionRequest<MintPairingCodeResponse>(base, "/me/device-pairing", {
    method: "POST",
    body: {},
  });
}

/** `POST /auth/device-pairing/begin` (unauthenticated, rate-limited) —
 * start enrolling a LOCAL passkey on this new device onto an existing account
 * referenced by the code. */
export function pairBegin(
  base: string,
  body: PairBeginRequest,
): Effect.Effect<RegisterBeginResponse, ArcheionFailure> {
  return archeionRequest<RegisterBeginResponse>(
    base,
    "/auth/device-pairing/begin",
    {
      method: "POST",
      body: body,
    },
  );
}

/** `POST /auth/device-pairing/finish` (unauthenticated) — verify + bind
 * the new passkey; mints a session for this device. */
export function pairFinish(
  base: string,
  body: PairFinishRequest,
): Effect.Effect<AuthFinishResponse, ArcheionFailure> {
  return archeionRequest<AuthFinishResponse>(
    base,
    "/auth/device-pairing/finish",
    {
      method: "POST",
      body: body,
    },
  );
}

// -- tagmata (unified pending + enrolled lifecycle) -----------------------

/** `POST /tagmata` — mint a pending tagma (an enrollment code). The
 * plaintext `code` is returned once. */
export function mintTagma(
  base: string,
): Effect.Effect<MintTagmaResponse, ArcheionFailure> {
  return archeionRequest<MintTagmaResponse>(base, "/tagmata", {
    method: "POST",
    body: {},
  });
}

/** `GET /tagmata` — the caller's tagmata (pending + enrolled, not revoked),
 * newest first. Registry view only; liveness is NOT included (it arrives via
 * the keryx `/me/events` stream). */
export function listTagmata(
  base: string,
): Effect.Effect<TagmaView[], ArcheionFailure> {
  return archeionRequest<TagmaView[]>(base, "/tagmata", {
    method: "GET",
  });
}

/** `PATCH /tagmata/{id}` — set or clear the label (pending or enrolled).
 * Returns on 204. */
export function renameTagma(
  base: string,
  id: string,
  label: string | null,
): Effect.Effect<void, ArcheionFailure> {
  const body: RenameTagmaRequest = { label };
  return archeionRequest<void>(base, `/tagmata/${encodeURIComponent(id)}`, {
    method: "PATCH",
    body: body,
  });
}

/** `DELETE /tagmata/{id}` — revoke (pending or enrolled). For an enrolled
 * tagma the archeion cuts the tagma off on its next request. Returns on 204. */
export function revokeTagma(
  base: string,
  id: string,
): Effect.Effect<void, ArcheionFailure> {
  return archeionRequest<void>(base, `/tagmata/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** `GET /tagmata/{id}` — the tagma's pinned Ed25519 device key (TOFU). The
 * app verifies the tagma's key-exchange signature against it. */
export function getTagma(
  base: string,
  id: string,
): Effect.Effect<TagmaInfo, ArcheionFailure> {
  return archeionRequest<TagmaInfo>(
    base,
    `/tagmata/${encodeURIComponent(id)}`,
    {
      method: "GET",
    },
  );
}

/** `GET /users/{username}` — a public user profile card. Public + per-IP
 * rate-limited; an unknown/disabled/malformed username 404s. */
export function getUserProfile(
  base: string,
  username: string,
): Effect.Effect<PublicUserProfile, ArcheionFailure> {
  return archeionRequest<PublicUserProfile>(
    base,
    `/users/${encodeURIComponent(username)}`,
    {
      method: "GET",
    },
  );
}

/** `GET /tagmata/{id}/profile` — a public tagma profile card. Public +
 * per-IP rate-limited; an unknown/pending/revoked tagma (or one whose owner
 * is disabled) 404s. */
export function getTagmaProfile(
  base: string,
  id: string,
): Effect.Effect<PublicTagmaProfile, ArcheionFailure> {
  return archeionRequest<PublicTagmaProfile>(
    base,
    `/tagmata/${encodeURIComponent(id)}/profile`,
    {
      method: "GET",
    },
  );
}

// -- credentials (personal access tokens + the tagma credential) --------

/** `GET /me/tokens` -- the caller's personal tokens, oldest first,
 *  revoked ones included (`revoked_at` set). The mask, never the
 *  secret: the plaintext is readable only from the create/reset
 *  response. */
export function listTokens(
  base: string,
): Effect.Effect<TokenView[], ArcheionFailure> {
  return archeionRequest<TokenView[]>(base, "/me/tokens", {
    method: "GET",
  });
}

/** `POST /me/tokens` -- mint a personal token. The response carries the
 *  plaintext once; the archeion keeps only its hash. */
export function createToken(
  base: string,
  body: CreateTokenRequest,
): Effect.Effect<IssuedToken, ArcheionFailure> {
  return archeionRequest<IssuedToken>(base, "/me/tokens", {
    method: "POST",
    body: body,
  });
}

/** `PATCH /me/tokens/{id}` -- rename and/or rescope. Returns the updated
 *  record; the next request with the token sees the new set. */
export function updateToken(
  base: string,
  id: string,
  body: UpdateTokenRequest,
): Effect.Effect<TokenView, ArcheionFailure> {
  return archeionRequest<TokenView>(
    base,
    `/me/tokens/${encodeURIComponent(id)}`,
    {
      method: "PATCH",
      body: body,
    },
  );
}

/** `DELETE /me/tokens/{id}` -- revoke. Idempotent, and the row stays in
 *  the list with `revoked_at` set. Returns on 204. */
export function revokeToken(
  base: string,
  id: string,
): Effect.Effect<void, ArcheionFailure> {
  return archeionRequest<void>(base, `/me/tokens/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/** `GET /tagmata/{id}/credential` -- the tagma's credential plus its
 *  history. Owner-session only. */
export function getTagmaCredential(
  base: string,
  id: string,
): Effect.Effect<TagmaCredentialView, ArcheionFailure> {
  return archeionRequest<TagmaCredentialView>(
    base,
    `/tagmata/${encodeURIComponent(id)}/credential`,
    {
      method: "GET",
    },
  );
}

/** `PATCH /tagmata/{id}/credential` -- rename and/or rescope the tagma
 *  credential. The change applies to the tagma's next request. */
export function updateTagmaCredential(
  base: string,
  id: string,
  body: TagmaCredentialUpdateRequest,
): Effect.Effect<TagmaCredentialView, ArcheionFailure> {
  return archeionRequest<TagmaCredentialView>(
    base,
    `/tagmata/${encodeURIComponent(id)}/credential`,
    {
      method: "PATCH",
      body: body,
    },
  );
}

/** `POST /tagmata/{id}/credential/reset` -- replace the tagma's secret.
 *  The old secret stops working on the tagma's next request; the new
 *  plaintext is returned once, for the operator to install. */
export function resetTagmaCredential(
  base: string,
  id: string,
): Effect.Effect<IssuedToken, ArcheionFailure> {
  return archeionRequest<IssuedToken>(
    base,
    `/tagmata/${encodeURIComponent(id)}/credential/reset`,
    {
      method: "POST",
      body: {},
    },
  );
}

/**
 * Control-plane client (the archeion service, default :7100): passkey ceremonies,
 * `/me`, and the tagma lifecycle. Also exposes `getTagma` -- the pinned device
 * key is TOFU from the control plane, even though the key exchange itself runs
 * on the keryx (see `@kallipai/kallipai-keryx-client`).
 *
 * The promise face of the verbs above, as this package offers it to consumers
 * (`kallipai-ui` is today's): one `runClientEffect` per call. Behaviour lives
 * in the verbs; a caller that can hold an Effect uses them directly. Do not
 * add behavior here. A rejection is the failure channel itself:
 * `ArcheionApiError` for a refusal, `TransportError` for a dead exchange.
 */
export class ArcheionClient {
  /** `base` is the archeion base, /v1/archeion included (the edge strips that
   * prefix before proxying), a trailing slash is trimmed. Paths are pure tails:
   * `/me`, `/tagmata/{id}`. */
  private readonly base: string;

  constructor(baseUrl: string) {
    // Trim a trailing slash so `${base}${path}` stays single-slash.
    this.base = baseUrl.replace(/\/+$/, "");
  }

  registerBegin(body: RegisterBeginRequest): Promise<RegisterBeginResponse> {
    return runClientEffect(registerBegin(this.base, body));
  }

  registerFinish(body: RegisterFinishRequest): Promise<AuthFinishResponse> {
    return runClientEffect(registerFinish(this.base, body));
  }

  loginBegin(body: LoginBeginRequest): Promise<LoginBeginResponse> {
    return runClientEffect(loginBegin(this.base, body));
  }

  loginFinish(body: LoginFinishRequest): Promise<AuthFinishResponse> {
    return runClientEffect(loginFinish(this.base, body));
  }

  adminLogin(key: string): Promise<AuthFinishResponse> {
    return runClientEffect(adminLogin(this.base, key));
  }

  usernameAvailability(
    username: string,
  ): Promise<UsernameAvailabilityResponse> {
    return runClientEffect(usernameAvailability(this.base, username));
  }

  loginDiscoverableBegin(): Promise<LoginBeginResponse> {
    return runClientEffect(loginDiscoverableBegin(this.base));
  }

  loginDiscoverableFinish(
    body: LoginFinishRequest,
  ): Promise<AuthFinishResponse> {
    return runClientEffect(loginDiscoverableFinish(this.base, body));
  }

  logout(): Promise<void> {
    return runClientEffect(logout(this.base));
  }

  listOAuthProviders(): Promise<ProviderInfo[]> {
    return runClientEffect(listOAuthProviders(this.base));
  }

  oauthSignInBegin(
    provider: string,
    body: { return_path?: string },
  ): Promise<OAuthBeginResponse> {
    return runClientEffect(oauthSignInBegin(this.base, provider, body));
  }

  oauthLinkBegin(
    provider: string,
    body: { return_path?: string },
  ): Promise<OAuthBeginResponse> {
    return runClientEffect(oauthLinkBegin(this.base, provider, body));
  }

  oauthFinish(
    provider: string,
    body: OAuthFinishRequest,
  ): Promise<AuthFinishResponse | OAuthNeedsUsernameResponse | undefined> {
    return runClientEffect(oauthFinish(this.base, provider, body));
  }

  oauthSignupComplete(
    body: OAuthSignupCompleteRequest,
  ): Promise<AuthFinishResponse> {
    return runClientEffect(oauthSignupComplete(this.base, body));
  }

  listExternalIdentities(): Promise<readonly ExternalIdentitySummary[]> {
    return runClientEffect(listExternalIdentities(this.base));
  }

  unlinkExternalIdentity(id: string): Promise<void> {
    return runClientEffect(unlinkExternalIdentity(this.base, id));
  }

  me(): Promise<MeResponse> {
    return runClientEffect(me(this.base));
  }

  listEmails(): Promise<EmailSummary[]> {
    return runClientEffect(listEmails(this.base));
  }

  addEmail(body: AddEmailRequest): Promise<EmailSummary> {
    return runClientEffect(addEmail(this.base, body));
  }

  verifyEmail(body: VerifyEmailRequest): Promise<EmailSummary> {
    return runClientEffect(verifyEmail(this.base, body));
  }

  makeEmailPrimary(id: string): Promise<EmailSummary> {
    return runClientEffect(makeEmailPrimary(this.base, id));
  }

  removeEmail(id: string): Promise<EmailSummary[]> {
    return runClientEffect(removeEmail(this.base, id));
  }

  listPasskeys(): Promise<PasskeySummary[]> {
    return runClientEffect(listPasskeys(this.base));
  }

  addPasskeyBegin(
    opts: { discoverable?: boolean } = {},
  ): Promise<RegisterBeginResponse> {
    return runClientEffect(addPasskeyBegin(this.base, opts));
  }

  addPasskeyFinish(body: AddPasskeyFinishRequest): Promise<PasskeySummary> {
    return runClientEffect(addPasskeyFinish(this.base, body));
  }

  renamePasskey(id: string, label: string): Promise<PasskeySummary> {
    return runClientEffect(renamePasskey(this.base, id, label));
  }

  revokePasskey(id: string): Promise<void> {
    return runClientEffect(revokePasskey(this.base, id));
  }

  mintPairingCode(): Promise<MintPairingCodeResponse> {
    return runClientEffect(mintPairingCode(this.base));
  }

  pairBegin(body: PairBeginRequest): Promise<RegisterBeginResponse> {
    return runClientEffect(pairBegin(this.base, body));
  }

  pairFinish(body: PairFinishRequest): Promise<AuthFinishResponse> {
    return runClientEffect(pairFinish(this.base, body));
  }

  mintTagma(): Promise<MintTagmaResponse> {
    return runClientEffect(mintTagma(this.base));
  }

  listTagmata(): Promise<TagmaView[]> {
    return runClientEffect(listTagmata(this.base));
  }

  renameTagma(id: string, label: string | null): Promise<void> {
    return runClientEffect(renameTagma(this.base, id, label));
  }

  revokeTagma(id: string): Promise<void> {
    return runClientEffect(revokeTagma(this.base, id));
  }

  getTagma(id: string): Promise<TagmaInfo> {
    return runClientEffect(getTagma(this.base, id));
  }

  getUserProfile(username: string): Promise<PublicUserProfile> {
    return runClientEffect(getUserProfile(this.base, username));
  }

  getTagmaProfile(id: string): Promise<PublicTagmaProfile> {
    return runClientEffect(getTagmaProfile(this.base, id));
  }

  listTokens(): Promise<TokenView[]> {
    return runClientEffect(listTokens(this.base));
  }

  createToken(body: CreateTokenRequest): Promise<IssuedToken> {
    return runClientEffect(createToken(this.base, body));
  }

  updateToken(id: string, body: UpdateTokenRequest): Promise<TokenView> {
    return runClientEffect(updateToken(this.base, id, body));
  }

  revokeToken(id: string): Promise<void> {
    return runClientEffect(revokeToken(this.base, id));
  }

  getTagmaCredential(id: string): Promise<TagmaCredentialView> {
    return runClientEffect(getTagmaCredential(this.base, id));
  }

  updateTagmaCredential(
    id: string,
    body: TagmaCredentialUpdateRequest,
  ): Promise<TagmaCredentialView> {
    return runClientEffect(updateTagmaCredential(this.base, id, body));
  }

  resetTagmaCredential(id: string): Promise<IssuedToken> {
    return runClientEffect(resetTagmaCredential(this.base, id));
  }
}
