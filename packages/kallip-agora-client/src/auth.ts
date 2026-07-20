// Ceremony drivers: run the begin -> `navigator.credentials.create|get` ->
// finish flow against an [`AgoraClient`], returning a typed result rather than
// throwing raw. The page decides how to render each `reason` (a user cancel is
// a soft hint, a 429 is a rate-limit message, etc.).

import type { AgoraClient } from "./http.ts";
import { AgoraApiError } from "./types.ts";
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
        | "duplicate-email"
        | "invalid-invite"
        | "unknown";
      message?: string;
    };

export interface RegisterArgs {
  readonly invite_code: string;
  readonly email: string;
  readonly username: string;
  readonly display_name?: string;
}

/** Run the registration ceremony (invite + email + username -> passkey -> finish). */
export async function registerWithPasskey(
  client: AgoraClient,
  args: RegisterArgs,
): Promise<CeremonyResult> {
  // begin: validate the invite + reserve the ceremony (HTTP failure -> typed).
  let ceremonyId: string;
  let options;
  try {
    const begun = await client.registerBegin({
      invite_code: args.invite_code,
      email: args.email,
      username: args.username,
      ...(args.display_name ? { display_name: args.display_name } : {}),
    });
    ceremonyId = begun.ceremony_id;
    options = begun.options;
  } catch (e) {
    return beginError(e, { 401: "invalid-invite", 429: "rate-limited" });
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

  // finish: bind the passkey + consume the invite (HTTP failure -> typed).
  // A 409 collides on either the email (login id) or the username (handle);
  // the agora emits the same status for both, distinguished only by message
  // text, so route 409 through classifyRegisterConflict before falling back
  // to the status-based mapper for everything else.
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

/** Run the login ceremony (email -> passkey -> finish). */
export async function loginWithPasskey(
  client: AgoraClient,
  email: string,
): Promise<CeremonyResult> {
  let ceremonyId: string;
  let options;
  try {
    const begun = await client.loginBegin({ email });
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
 * The browser ceremony threw. `NotAllowedError`/`AbortError` = the user
 * cancelled the platform prompt; anything else is `unknown`.
 */
function cancelOrUnknown(e: unknown): CeremonyResult {
  if (
    e instanceof DOMException &&
    (e.name === "NotAllowedError" || e.name === "AbortError")
  ) {
    return { ok: false, reason: "cancelled" };
  }
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
  | "duplicate-email"
  | "invalid-invite"
  | "unknown";

/**
 * Map a register-finish 409 message to a typed reason. The agora emits the
 * SAME 409 for both email and username collisions, distinguished only by
 * message text (`crates/kallip-agora/src/routes/auth.rs` returns
 * "email already registered" / "username already taken"). Match the EXACT
 * server strings so a contract drift surfaces as a visible "unknown" rather
 * than a silent misclassification. The stable long-term fix belongs at the
 * agora layer (a stable `code` field on `ApiError`, not prose) -- flagged as
 * coupling debt to revisit there.
 *
 * Exported (but not re-exported from index.ts) so the contract strings are
 * pinned by `auth_test.ts`; an agora copy change must update this switch.
 */
export function classifyRegisterConflict(message: string): CeremonyReason {
  switch (message) {
    case "email already registered":
      return "duplicate-email";
    case "username already taken":
      return "duplicate-username";
    default:
      return "unknown";
  }
}
