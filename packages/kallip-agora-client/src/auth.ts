// Ceremony drivers: run the begin -> `navigator.credentials.create|get` ->
// finish flow against an [`AgoraClient`], returning a typed result rather than
// throwing raw. The page decides how to render each `reason` (a user cancel is
// a soft hint, a 429 is a rate-limit message, etc.).

import type { AgoraClient } from "./http.ts";
import { AgoraApiError, type PasskeySummary } from "./types.ts";
import {
  loginCredentialToJson,
  optionsForCreate,
  optionsForGet,
  registerCredentialToJson,
} from "./webauthn.ts";

/** The outcome of a passkey ceremony. */
export type CeremonyResult =
  | { ok: true; userId: string }
  | {
    ok: false;
    reason:
      | "cancelled"
      | "rate-limited"
      | "duplicate-username"
      | "unknown";
    message?: string;
  };

export interface RegisterArgs {
  readonly username: string;
  readonly display_name?: string;
}

/** Run the registration ceremony (username -> passkey -> finish). */
export async function registerWithPasskey(
  client: AgoraClient,
  args: RegisterArgs,
): Promise<CeremonyResult> {
  // begin: reserve the ceremony (HTTP failure -> typed).
  let ceremonyId: string;
  let options;
  try {
    const begun = await client.registerBegin({
      username: args.username,
      ...(args.display_name ? { display_name: args.display_name } : {}),
    });
    ceremonyId = begun.ceremony_id;
    options = begun.options;
  } catch (e) {
    return beginError(e, { 429: "rate-limited" });
  }

  // create: the browser passkey prompt (cancel/abort -> "cancelled").
  let credential: PublicKeyCredential | null;
  try {
    credential = (await navigator.credentials.create({
      publicKey: optionsForCreate(options),
    })) as PublicKeyCredential | null;
  } catch (e) {
    return cancelOrUnknown(e);
  }

  // finish: bind the passkey (HTTP failure -> typed). A 409 is a username
  // collision (the agora emits "username already taken"); route 409 through
  // classifyRegisterConflict before falling back to the status-based mapper for
  // everything else.
  try {
    const finish = await client.registerFinish({
      ceremony_id: ceremonyId,
      credential: registerCredentialToJson(credential),
    });
    return { ok: true, userId: finish.user_id };
  } catch (e) {
    if (e instanceof AgoraApiError && e.status === 409) {
      return {
        ok: false,
        reason: classifyRegisterConflict(e.message),
        message: e.message,
      };
    }
    return finishError(e, { 429: "rate-limited" });
  }
}

/** Run the login ceremony (username -> passkey -> finish). */
export async function loginWithPasskey(
  client: AgoraClient,
  username: string,
): Promise<CeremonyResult> {
  let ceremonyId: string;
  let options;
  try {
    const begun = await client.loginBegin({ username });
    ceremonyId = begun.ceremony_id;
    options = begun.options;
  } catch (e) {
    return beginError(e, { 429: "rate-limited" });
  }

  let credential: PublicKeyCredential | null;
  try {
    credential = (await navigator.credentials.get({
      publicKey: optionsForGet(options),
    })) as PublicKeyCredential | null;
  } catch (e) {
    return cancelOrUnknown(e);
  }

  try {
    const finish = await client.loginFinish({
      ceremony_id: ceremonyId,
      credential: loginCredentialToJson(credential),
    });
    return { ok: true, userId: finish.user_id };
  } catch (e) {
    return finishError(e, { 429: "rate-limited" });
  }
}

/**
 * Run the discoverable (usernameless) login ceremony. No identifier is supplied:
 * the authenticator surfaces matching resident credentials via conditional-UI
 * autofill, and the server resolves the account from the assertion's
 * `userHandle`. The browser must support conditional mediation; on unsupported
 * browsers or when no discoverable credential exists, the call resolves with a
 * non-ok result (typically `cancelled`) and the caller falls back to
 * `loginWithPasskey`.
 */
export async function loginWithDiscoverablePasskey(
  client: AgoraClient,
  signal?: AbortSignal,
): Promise<CeremonyResult> {
  let ceremonyId: string;
  let options;
  try {
    const begun = await client.loginDiscoverableBegin();
    ceremonyId = begun.ceremony_id;
    options = begun.options;
  } catch (e) {
    return beginError(e, { 429: "rate-limited" });
  }

  let credential: PublicKeyCredential | null;
  try {
    // `mediation: "conditional"` MUST be on the outer `credentials.get` call (it
    // is dropped from the server `RequestChallengeResponse` by this client's
    // `ServerRequestOptions` model, which only carries `publicKey`). `signal`
    // lets the caller abort a pending conditional get BEFORE starting an
    // explicit ceremony (e.g. the username form submit) -- two concurrent
    // `credentials.get()` calls deadlock some browsers (notably Firefox).
    credential = (await navigator.credentials.get({
      publicKey: optionsForGet(options),
      mediation: "conditional",
      signal,
    })) as PublicKeyCredential | null;
  } catch (e) {
    return cancelOrUnknown(e);
  }
  // A null resolve is a no-selection cancel variant: classify it as `cancelled`
  // (silent fallback) rather than letting it throw inside the finish try below
  // and surface as a confusing `unknown`.
  if (!credential) return { ok: false, reason: "cancelled" };

  try {
    const finish = await client.loginDiscoverableFinish({
      ceremony_id: ceremonyId,
      credential: loginCredentialToJson(credential),
    });
    return { ok: true, userId: finish.user_id };
  } catch (e) {
    return finishError(e, { 429: "rate-limited" });
  }
}

/**
 * Start an OAuth sign-in (or link) ceremony: fetch the provider's authorize URL
 * and navigate the browser there. The provider redirects back to the SPA
 * callback with `code`+`state`, which [`completeOAuth`] exchanges.
 *
 * This NAVIGATES AWAY (full page navigation); it has no return value. The
 * ceremony completes on the callback page after the redirect.
 */
export async function signInWithOAuth(
  client: AgoraClient,
  provider: string,
  opts: { returnPath?: string } = {},
): Promise<void> {
  const { authorize_url } = await client.oauthSignInBegin(provider, {
    return_path: opts.returnPath,
  });
  window.location.href = authorize_url;
}

/** The outcome of an OAuth finish on the SPA callback page. The same `/finish`
 * endpoint serves both actions (the opaque `state` selects which): a signin
 * mints a session (200/201, `kind: "signin"`); a link binds an identity to the
 * already-signed-in account (204, `kind: "link"`); an UNLINKED identity holds
 * the claim for a username step (202, `kind: "needs-username"`). The callback
 * page navigates by `kind` -- signin resumes to `returnPath`/home + refreshes
 * the session, link returns to settings + refreshes identities, needs-username
 * routes to the OAuth signup page carrying the held token. */
export type OAuthCompleteResult =
  | { ok: true; kind: "signin"; userId: string; returnPath?: string }
  | { ok: true; kind: "link" }
  | {
    ok: true;
    kind: "needs-username";
    signupToken: string;
    provider: string;
  }
  | {
    ok: false;
    reason: "rate-limited" | "unknown";
    message?: string;
  };

/**
 * Complete an OAuth ceremony on the SPA callback page: post the provider's
 * `code`+`state` to the agora. A 204 (no body) is a successful link; a 200/201
 * body is a successful signin (cookie set via this same-site credentialed XHR),
 * carrying the user id + the sanitized return path to resume to; a 202 body
 * `kind:"needs-username"` holds the resolved claim -- the SPA collects a chosen
 * username and submits it via {@link completeOAuthSignup}. Any failure (provider
 * unavailable, invalid state) surfaces as a generic `unknown` -- the callback
 * page shows a neutral message + a return-to-login link.
 */
export async function completeOAuth(
  client: AgoraClient,
  provider: string,
  body: { state: string; code: string },
): Promise<OAuthCompleteResult> {
  try {
    const finish = await client.oauthFinish(provider, body);
    // 204 (link) resolves to `undefined`.
    if (!finish) return { ok: true, kind: "link" };
    // 202 (needs-username) carries signup_token; 200/201 (signin) carries user_id.
    if ("signup_token" in finish) {
      return {
        ok: true,
        kind: "needs-username",
        signupToken: finish.signup_token,
        provider: finish.provider,
      };
    }
    return {
      ok: true,
      kind: "signin",
      userId: finish.user_id,
      ...(finish.return_path ? { returnPath: finish.return_path } : {}),
    };
  } catch (e) {
    if (e instanceof AgoraApiError) {
      if (e.status === 429) {
        return { ok: false, reason: "rate-limited", message: e.message };
      }
      return { ok: false, reason: "unknown", message: e.message };
    }
    return {
      ok: false,
      reason: "unknown",
      message: e instanceof Error ? e.message : String(e),
    };
  }
}

/** The outcome of completing a held OAuth signup with a chosen username. */
export type OAuthSignupResult =
  | { ok: true; userId: string; returnPath?: string }
  | {
    ok: false;
    reason:
      | "duplicate-username"
      | "invalid-username"
      | "signup-disabled"
      | "rate-limited"
      | "unknown";
    message?: string;
  };

/**
 * Submit the chosen username for a held OAuth signup (the token came from a
 * `needs-username` finish). On success the session cookie is set by this
 * credentialed XHR; the caller refreshes the session and navigates to
 * `returnPath`/home. A taken username surfaces as `duplicate-username` so the
 * form can prompt for another (the held row survives for retry). A 403 is the
 * operator kill-switch (`signup_enabled` off) -> `signup-disabled`.
 */
export async function completeOAuthSignup(
  client: AgoraClient,
  body: { signupToken: string; username: string },
): Promise<OAuthSignupResult> {
  try {
    const finish = await client.oauthSignupComplete({
      signup_token: body.signupToken,
      username: body.username,
    });
    return {
      ok: true,
      userId: finish.user_id,
      ...(finish.return_path ? { returnPath: finish.return_path } : {}),
    };
  } catch (e) {
    if (e instanceof AgoraApiError) {
      if (e.status === 409 && e.message === "username already taken") {
        return { ok: false, reason: "duplicate-username", message: e.message };
      }
      if (e.status === 400) {
        return { ok: false, reason: "invalid-username", message: e.message };
      }
      if (e.status === 403) {
        return { ok: false, reason: "signup-disabled", message: e.message };
      }
      if (e.status === 429) {
        return { ok: false, reason: "rate-limited", message: e.message };
      }
      return { ok: false, reason: "unknown", message: e.message };
    }
    return {
      ok: false,
      reason: "unknown",
      message: e instanceof Error ? e.message : String(e),
    };
  }
}

/** The outcome of binding another passkey to the signed-in account. */
export type AddPasskeyResult =
  | { ok: true; passkey: PasskeySummary }
  | {
    ok: false;
    reason:
      | "cancelled"
      | "reauth-required"
      | "duplicate-credential"
      | "rate-limited"
      | "unknown";
    message?: string;
  };

export interface AddPasskeyArgs {
  /** The signed-in user's username (login id); used for the passkey step-up
   * re-login when the agora returns `reauth-required`. OMIT for an OAuth-only
   * account (no passkey to re-auth with): the driver then returns
   * `reauth-required` and the UI re-establishes the step-up via OAuth (which
   * navigates away and back). */
  readonly username?: string;
  /** The device label for the new passkey. */
  readonly label: string;
  /** When true, enroll a discoverable (resident-key / passwordless) credential
   * instead of a regular passkey. */
  readonly discoverable?: boolean;
}

/**
 * Bind ANOTHER passkey to the signed-in account (a second-device ceremony).
 *
 * One-shot step-up: if `addPasskeyBegin` returns 403 `reauth-required`, run the
 * login ceremony (the user touches an existing passkey to prove presence), then
 * retry begin exactly once. Requires `args.username` (a passkey account); an
 * OAuth-only account omits it and gets `reauth-required` so the UI can
 * re-establish the step-up via OAuth. The typed label threads through the
 * re-auth so the user does not re-type it.
 */
export async function addPasskey(
  client: AgoraClient,
  args: AddPasskeyArgs,
): Promise<AddPasskeyResult> {
  // begin (with one step-up retry on reauth-required).
  const beginOpts = args.discoverable ? { discoverable: true } : {};
  let begun;
  try {
    begun = await client.addPasskeyBegin(beginOpts);
  } catch (e) {
    if (!(e instanceof AgoraApiError) || e.status !== 403) {
      return mapAddBeginError(e);
    }
    // OAuth-only accounts (no username) cannot re-auth via a passkey: surface
    // reauth-required so the UI re-establishes the step-up via OAuth.
    if (!args.username) {
      return { ok: false, reason: "reauth-required" };
    }
    // Step-up: run the login ceremony, then retry begin once.
    const login = await loginWithPasskey(client, args.username);
    if (!login.ok) {
      // Forward the step-up login's own reason when it is one an add can surface
      // (a cancel, or a rate-limit on the re-auth); anything else means the
      // re-auth itself failed to establish the step-up.
      if (login.reason === "cancelled" || login.reason === "rate-limited") {
        return { ok: false, reason: login.reason, message: login.message };
      }
      return { ok: false, reason: "reauth-required", message: login.message };
    }
    try {
      begun = await client.addPasskeyBegin(beginOpts);
    } catch (e2) {
      return mapAddBeginError(e2);
    }
  }

  // create: the browser passkey prompt (cancel/abort -> "cancelled").
  let credential: PublicKeyCredential | null;
  try {
    credential = (await navigator.credentials.create({
      publicKey: optionsForCreate(begun.options),
    })) as PublicKeyCredential | null;
  } catch (e) {
    return addCancelOrUnknown(e);
  }

  // finish: verify + bind. A 409 is a duplicate credential (live or previously
  // revoked); the last-live-passkey guard never fires on an ADD (the user always
  // has at least the one they are adding to).
  try {
    const passkey = await client.addPasskeyFinish({
      ceremony_id: begun.ceremony_id,
      credential: registerCredentialToJson(credential),
      label: args.label,
    });
    return { ok: true, passkey };
  } catch (e) {
    if (e instanceof AgoraApiError && e.status === 409) {
      return { ok: false, reason: "duplicate-credential", message: e.message };
    }
    if (e instanceof AgoraApiError && e.status === 429) {
      return { ok: false, reason: "rate-limited", message: e.message };
    }
    return addUnknown(e);
  }
}

/** `NotAllowedError`/`AbortError` during an add-passkey prompt = cancelled. */
function addCancelOrUnknown(e: unknown): AddPasskeyResult {
  if (isUserCancel(e)) return { ok: false, reason: "cancelled" };
  return addUnknown(e);
}

function addUnknown(e: unknown): AddPasskeyResult {
  const message = e instanceof Error ? e.message : String(e);
  return { ok: false, reason: "unknown", message };
}

/** Map an add-passkey begin failure (other than the 403 handled by the caller). */
function mapAddBeginError(e: unknown): AddPasskeyResult {
  if (e instanceof AgoraApiError) {
    if (e.status === 403) {
      return { ok: false, reason: "reauth-required", message: e.message };
    }
    if (e.status === 429) {
      return { ok: false, reason: "rate-limited", message: e.message };
    }
  }
  const message = e instanceof Error ? e.message : String(e);
  return { ok: false, reason: "unknown", message };
}

// ---------------------------------------------------------------------------
// pair a new device (cross-device enrollment via a short-lived code)
// ---------------------------------------------------------------------------

/** The outcome of pairing a new device: bind a local passkey onto an existing
 *  account and sign this device in. */
export type PairResult =
  | { ok: true; userId: string }
  | {
    ok: false;
    reason:
      | "cancelled"
      | "invalid-code"
      | "duplicate-credential"
      | "rate-limited"
      | "unknown";
    message?: string;
  };

export interface PairArgs {
  /** The short pairing code from the already-signed-in device. */
  readonly code: string;
  /** The device label for the new passkey. */
  readonly label: string;
}

/**
 * Pair THIS new device onto an existing account using a short code minted by an
 * already-signed-in device. No step-up here (the code IS the proof of account
 * ownership). On success this device is signed in with its own local passkey.
 */
export async function pairDevice(
  client: AgoraClient,
  args: PairArgs,
): Promise<PairResult> {
  // begin: validate the code (the server emits a uniform 401 for
  // unknown/consumed/expired, so all map to one `invalid-code` reason).
  let begun;
  try {
    begun = await client.pairBegin({ code: args.code });
  } catch (e) {
    if (e instanceof AgoraApiError) {
      if (e.status === 401 || e.status === 403) {
        return { ok: false, reason: "invalid-code", message: e.message };
      }
      if (e.status === 429) {
        return { ok: false, reason: "rate-limited", message: e.message };
      }
    }
    return pairUnknown(e);
  }

  // create: the browser passkey prompt (cancel/abort -> "cancelled").
  let credential: PublicKeyCredential | null;
  try {
    credential = (await navigator.credentials.create({
      publicKey: optionsForCreate(begun.options),
    })) as PublicKeyCredential | null;
  } catch (e) {
    if (isUserCancel(e)) return { ok: false, reason: "cancelled" };
    return pairUnknown(e);
  }

  // finish: verify + bind + mint session. A 409 is a duplicate credential.
  try {
    const finish = await client.pairFinish({
      ceremony_id: begun.ceremony_id,
      credential: registerCredentialToJson(credential),
      label: args.label,
    });
    return { ok: true, userId: finish.user_id };
  } catch (e) {
    if (e instanceof AgoraApiError && e.status === 409) {
      return { ok: false, reason: "duplicate-credential", message: e.message };
    }
    if (e instanceof AgoraApiError && e.status === 429) {
      return { ok: false, reason: "rate-limited", message: e.message };
    }
    // 401 "ceremony expired" (the user sat on the prompt past its TTL) collapses
    // to the same `invalid-code` UX as begin -- "code invalid/expired/used".
    if (e instanceof AgoraApiError && e.status === 401) {
      return { ok: false, reason: "invalid-code", message: e.message };
    }
    return pairUnknown(e);
  }
}

function pairUnknown(e: unknown): PairResult {
  const message = e instanceof Error ? e.message : String(e);
  return { ok: false, reason: "unknown", message };
}

// ---------------------------------------------------------------------------
// error mapping
// ---------------------------------------------------------------------------

/** Map a ceremony-begin failure (HTTP) to a typed reason via status code. */
function beginError(
  e: unknown,
  map: Readonly<Record<number, CeremonyReason>>,
): CeremonyResult {
  if (e instanceof AgoraApiError) {
    const reason = map[e.status];
    if (reason) return { ok: false, reason, message: e.message };
  }
  return unknownError(e);
}

/** Map a ceremony-finish failure (HTTP) to a typed reason via status code. */
function finishError(
  e: unknown,
  map: Readonly<Record<number, CeremonyReason>>,
): CeremonyResult {
  if (e instanceof AgoraApiError) {
    const reason = map[e.status];
    if (reason) return { ok: false, reason, message: e.message };
  }
  return unknownError(e);
}

/**
 * Did the browser ceremony throw a user-cancel? `NotAllowedError`/`AbortError`
 * = the user dismissed the platform prompt. Shared by both ceremony drivers.
 */
function isUserCancel(e: unknown): boolean {
  return (
    e instanceof DOMException &&
    (e.name === "NotAllowedError" || e.name === "AbortError")
  );
}

/**
 * The browser ceremony threw. `NotAllowedError`/`AbortError` = the user
 * cancelled the platform prompt; anything else is `unknown`.
 */
function cancelOrUnknown(e: unknown): CeremonyResult {
  if (isUserCancel(e)) return { ok: false, reason: "cancelled" };
  return unknownError(e);
}

function unknownError(e: unknown): CeremonyResult {
  const message = e instanceof Error ? e.message : String(e);
  return { ok: false, reason: "unknown", message };
}

type CeremonyReason =
  | "cancelled"
  | "rate-limited"
  | "duplicate-username"
  | "unknown";

/**
 * Map a register-finish 409 message to a typed reason. Registration collects no
 * email, so the only collision is the username (the agora emits
 * "username already taken"). Match the EXACT server string so a contract drift
 * surfaces as a visible "unknown" rather than a silent misclassification. The
 * stable long-term fix belongs at the agora layer (a stable `code` field on
 * `ApiError`, not prose) -- flagged as coupling debt to revisit there.
 *
 * Exported (but not re-exported from index.ts) so the contract string is
 * pinned by `auth_test.ts`; an agora copy change must update this switch.
 */
export function classifyRegisterConflict(message: string): CeremonyReason {
  switch (message) {
    case "username already taken":
      return "duplicate-username";
    default:
      return "unknown";
  }
}
