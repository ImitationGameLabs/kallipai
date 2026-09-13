// ArcheionSessionStore: reactive ($state) wrapper around the archeion client holding
// the signed-in user and the owner's tagmata across their lifecycle (pending ->
// enrolled -> revoked).
//
// Error discipline: auth-fatal errors (whoami/register/login/logout) live in
// `authError` and clear `user`; the list error lives in `tagmataError` so a
// fetch failure never blanks the signed-in state or vice-versa. `user` is
// tri-state: `undefined` = unresolved (the root
// layout is still calling whoami), `null` = resolved logged-out, `MeResponse` =
// signed in. The auth gate treats only `null` as "redirect to /login", so a
// transient network failure (user stays undefined) renders a skeleton rather
// than booting the user out.
// The *Error fields carry qualitative, localized copy only -- a raw error's
// message/stack is internal detail and goes to the console, never the UI.
//
// The archeion base URL is injected via initArcheion() at app bootstrap -- this
// package does not read import.meta.env (which is only typed in a SvelteKit
// app, not a library).

import { copyText } from "../clipboard.ts";
import {
  addPasskey,
  adminLoginWithKey,
  type AdminLoginResult,
  type AddPasskeyResult,
  ArcheionApiError,
  ArcheionClient,
  type CeremonyResult,
  completeOAuth,
  completeOAuthSignup,
  type ExternalIdentitySummary,
  loginWithDiscoverablePasskey,
  loginWithPasskey,
  type MeResponse,
  type OAuthCompleteResult,
  type OAuthSignupResult,
  pairDevice,
  type PairResult,
  type PasskeySummary,
  type ProviderInfo,
  type ProviderSummary,
  type ProviderRequest,
  registerWithPasskey,
  type TagmaView,
} from "@kallipai/kallip-archeion-client";
import type { PairingCodeView } from "../passkeys.svelte.ts";
import { INSTANCES_TOKEN_KEY } from "../instances/client.ts";
import { LescheClient } from "@kallipai/kallip-lesche-client";
import { FilesClient } from "@kallipai/kallip-files-client";
import {
  auth_couldnt_reach,
  auth_no_signed_in_user,
  auth_passkey_cancelled,
  oauth_context_lost,
  pair_reauth_failed,
  auth_reauth_required,
  rooms_couldnt_reach,
} from "../../paraglide/messages.js";
import { participantIdForUser } from "@kallipai/kallip-common";
import type {
  EnrollmentCodeCardProps,
  TagmaCardProps,
} from "../tagmata.svelte.ts";
import { open, seal } from "../vault/crypto.ts";
import { deviceVault } from "../vault/keyStore.ts";

let archeionClient: ArcheionClient | null = null;
let archeionBaseUrl = "";

/** Inject the archeion base URL and construct the client. Called once at bootstrap. */
export function initArcheion(url: string): void {
  archeionClient = new ArcheionClient(url);
  archeionBaseUrl = url;
}

function client(): ArcheionClient {
  if (!archeionClient) {
    throw new Error("initArcheion(url) must be called at app bootstrap");
  }
  return archeionClient;
}

/** The control-plane (archeion) client; throws if initArcheion has not been called.
 * Exposed so peer stores (e.g. the channels store, which needs `getTagma` for
 * the key-exchange's pinned key) can reach the same singleton. */
export function archeionClientOrFail(): ArcheionClient {
  return client();
}
/** The injected archeion base URL. Throws pre-init. */
export function archeionBaseUrlOrFail(): string {
  if (!archeionBaseUrl) {
    throw new Error("initArcheion(url) must be called at app bootstrap");
  }
  return archeionBaseUrl;
}
/** The polis edge origin behind the injected archeion base URL (the
 * base with its /v1/archeion tail stripped) -- what one-click instance
 * spawn relays to the new tagma as its KALLIP_POLIS_URL bootstrap env.
 * Throws pre-init, or on a base without the expected tail (a bootstrap
 * wiring bug, not a runtime condition). */
export function archeionPolisOriginOrFail(): string {
  const base = archeionBaseUrlOrFail();
  const tail = "/v1/archeion";
  if (!base.endsWith(tail)) {
    throw new Error(`archeion base URL ${base} lacks the ${tail} tail`);
  }
  return base.slice(0, base.length - tail.length);
}

// The lesche (data-plane) client lives on a separate origin from the archeion; its
// URL is injected the same way (no import.meta.env in this library). The session
// cookie is shared cross-subdomain, so the same credentialed fetch works.
let lescheClient: LescheClient | null = null;
let lescheBaseUrl = "";

/** Inject the lesche base URL and construct the data-plane client. Called once
 * at bootstrap alongside initArcheion. */
export function initLesche(url: string): void {
  lescheClient = new LescheClient(url);
  lescheBaseUrl = url;
}

/** The data-plane (lesche) client; throws if initLesche has not been called.
 * Consumed by channelsStore (the key exchange + envelope relay) and
 * realtimeStore (the me/events SSE) -- both peer singletons. */
export function lescheClientOrFail(): LescheClient {
  if (!lescheClient) {
    throw new Error("initLesche(url) must be called at app bootstrap");
  }
  return lescheClient;
}

// The files (transfer-plane) client lives on its own origin too; its URL is
// injected the same way (no import.meta.env in this library). The session
// cookie is shared cross-subdomain, so the same credentialed fetch works.
let filesClient: FilesClient | null = null;

/** Inject the files base URL and construct the transfer client. Called once
 * at bootstrap alongside initArcheion/initLesche. */
export function initFiles(url: string): void {
  filesClient = new FilesClient(url);
}

/** The files (transfer-plane) client; throws if initFiles has not been
 * called. Consumed by the chat page's attachment orchestration (shared
 * upload -> inbox delivery -> message reference). */
export function filesClientOrFail(): FilesClient {
  if (!filesClient) {
    throw new Error("initFiles(url) must be called at app bootstrap");
  }
  return filesClient;
}
/** The injected lesche base URL; same relay purpose as the archeion one. */
export function lescheBaseUrlOrFail(): string {
  if (!lescheBaseUrl) {
    throw new Error("initLesche(url) must be called at app bootstrap");
  }
  return lescheBaseUrl;
}

/** sessionStorage key stashing the in-flight OAuth provider + action across the
 * provider redirect (the callback URL carries only `code`+`state`, not which
 * provider began the ceremony). Set at begin, consumed at finish. */
const OAUTH_CALLBACK_KEY = "kallip.oauth.callback";

/** sessionStorage key for the passkey-login marker: set on a successful
 * passkey register/login/pair, cleared on any other login path. Tab-scoped by
 * storage type, which matches the "logged in with a passkey THIS time" bar:
 * the vault mode-flip affordance shows only while the marker is live. NOT a
 * security boundary -- flipping encrypted->plaintext requires opening the
 * blob with this device's vault key, so a forged marker is self-damage only. */
const VIA_PASSKEY_KEY = "kallip.session.via-passkey";

/** True when THIS login came through a passkey ceremony (see the key doc). */
function viaPasskey(): boolean {
  try {
    return sessionStorage.getItem(VIA_PASSKEY_KEY) === "1";
  } catch {
    return false; // sessionStorage unavailable (SSR / privacy mode)
  }
}

/** Mark the current session as arrived via passkey (set on success). */
function markViaPasskey(): void {
  try {
    sessionStorage.setItem(VIA_PASSKEY_KEY, "1");
  } catch {
    // Storage write failed; the flip affordance stays hidden (fail closed).
  }
}

/** Clear the passkey marker -- any non-passkey login path calls this. */
function clearViaPasskey(): void {
  try {
    sessionStorage.removeItem(VIA_PASSKEY_KEY);
  } catch {
    // Nothing to clean up if storage is unavailable.
  }
}

interface OAuthCallbackContext {
  provider: string;
  action: "signin" | "link";
}

/** Stash the provider/action before navigating to the provider authorize URL. */
function stashOAuthCallback(ctx: OAuthCallbackContext): void {
  try {
    sessionStorage.setItem(OAUTH_CALLBACK_KEY, JSON.stringify(ctx));
  } catch {
    // sessionStorage unavailable (private mode) -> the callback cannot complete;
    // it surfaces a clear error. Best-effort.
  }
}

/** sessionStorage key stashing the held OAuth signup token between the callback
 * page and the username step. The token is a single-use bearer that grants
 * account creation bound to a fixed OAuth identity, so it rides sessionStorage
 * (not the URL -- a query string would leak into history/access logs/Referer).
 * Set on the callback page, PEEKED on the signup page (so a refresh during the
 * flow survives), and cleared on success. The server-side row is the real
 * single-use authority, so leaving the stash until success is safe. */
const OAUTH_SIGNUP_KEY = "kallip.oauth.signup";

export interface OAuthSignupContext {
  signupToken: string;
  provider: string;
}
/** Stash the held signup token so the username page can complete the signup. */
export function stashOAuthSignup(ctx: OAuthSignupContext): void {
  try {
    sessionStorage.setItem(OAUTH_SIGNUP_KEY, JSON.stringify(ctx));
  } catch {
    // sessionStorage unavailable -> the username page cannot complete; it
    // surfaces a clear error. Best-effort.
  }
}

/** Peek the stashed signup token WITHOUT removing it, so a page refresh during
 * the username step (or between duplicate-username retries) keeps working --
 * the token is only cleared on success via {@link clearOAuthSignup}. Returns
 * null when none is stashed (direct navigation, or refresh after success). */
export function peekOAuthSignup(): OAuthSignupContext | null {
  try {
    const raw = sessionStorage.getItem(OAUTH_SIGNUP_KEY);
    return raw ? (JSON.parse(raw) as OAuthSignupContext) : null;
  } catch {
    return null;
  }
}

/** Drop the stashed signup token -- call on a successful complete so a later
 * refresh bounces to /register instead of re-rendering a dead form. */
export function clearOAuthSignup(): void {
  try {
    sessionStorage.removeItem(OAUTH_SIGNUP_KEY);
  } catch {
    // Best-effort.
  }
}
class ArcheionSessionStore {
  // Tri-state: undefined = unresolved, null = logged out, MeResponse = signed in.
  //
  // Invariant: this field is only meaningful in online mode. The archeion session
  // cookie survives offline mode (we never logout() on a mode switch), so `user`
  // can remain a stale MeResponse while the app is in offline mode. Offline UI
  // must not branch on it -- the status snippet, nav, and gate are all
  // mode-gated, so nothing in offline mode reads `user`. Do not change that
  // without adding a guard.
  user: MeResponse | null | undefined = $state(undefined);

  // The signed-in user's opaque room-layer participant id (`ParticipantId::
  // for_user`). The room envelope sender + the `mine` flag derive from it, so it
  // is resolved once per session (in whoami) alongside `user`. Null while
  // logged out / unresolved. NOT the raw user_id: the lesche's room routes
  // authenticate the sender against this derived id, so a raw user_id would 403.
  participantId: string | null = $state(null);

  // Split errors (see file comment).
  authError: string | null = $state(null);
  tagmataError: string | null = $state(null);

  // Raw ceremony result for the auth pages to render inline.
  lastCeremony: CeremonyResult | null = $state(null);

  minting = $state(false);
  copiedCodeId: string | null = $state(null);

  // The owner's tagmata (pending + enrolled; revoked are never listed), newest
  // first. The archeion owns code masking; this store holds no separate secret
  // cache beyond the transient `mintedCode` (the once-shown plaintext).
  tagmata: TagmaView[] = $state([]);
  tagmataLoaded = $state(false);

  // The signed-in user's live passkeys (revoked history is not listed). Mirrors
  // the tagmata error discipline: a list fetch failure lands in `passkeysError`
  // and never blanks `user`; rename/revoke THROW so a single failure does not
  // blank the whole section.
  passkeys: PasskeySummary[] = $state([]);
  passkeysError: string | null = $state(null);
  passkeysLoaded = $state(false);
  // Result of the last add-device ceremony, for the settings page to render
  // inline (success / reauth-required / duplicate / cancelled).
  lastAddPasskey: AddPasskeyResult | null = $state(null);

  // The signed-in user's linked OAuth identities. Mirrors the passkeys
  // discipline: a refresh failure lands in `externalIdentitiesError` and never
  // blanks `user`; unlink THROWS so one failure does not blank the section.
  externalIdentities: ExternalIdentitySummary[] = $state([]);
  externalIdentitiesError: string | null = $state(null);
  externalIdentitiesLoaded = $state(false);

  // Which OAuth providers this deploy has configured (for the login/register
  // buttons + the settings link affordance). Unauthenticated; fetched by the
  // login/register/settings pages on mount. Best-effort: a fetch failure leaves
  // the list empty. `oauthProvidersLoaded` guards the settings effect; it is set
  // ONLY on success, so a transient failure leaves it false and the effect
  // refetches on the next mount (recovering a /login-time blip rather than
  // hiding providers for the session). Guard on the flag, not on length.
  oauthProviders: ProviderInfo[] = $state([]);
  oauthProvidersLoaded = $state(false);

  // The signed-in user's provider vault (API keys; encrypted rows are blobs
  // only this device can open). Mirrors the passkeys discipline: a list
  // failure lands in `providersError` and never blanks `user`; mutations
  // THROW so one failure does not blank the section.
  providers: ProviderSummary[] = $state([]);
  providersError: string | null = $state(null);
  providersLoaded = $state(false);
  // A freshly minted device-pairing code, shown once (typeable + QR) with a
  // countdown, for a new device to redeem. Step-up-gated like addPasskey.
  pairingCode: PairingCodeView | null = $state(null);
  pairingError: string | null = $state(null);
  // Result of the last pair-a-new-device ceremony run from an anonymous device.
  lastPair: PairResult | null = $state(null);

  // The plaintext of just-minted pending tagmas, shown once on the new card
  // (transient -- dropped on the next refresh, when the archeion's masked value
  // takes over). Keyed by tagma id.
  private mintedCode: Record<string, string> = {};

  /** Pending tagmata as card props. `code` is the just-minted full plaintext
   *  while `mintedCode` holds it (the only chance to copy); otherwise the archeion's
   *  masked `code_masked`. base64url bodies and the `sk-enroll-` prefix contain
   *  no `*`, so the masked form's `***` is an unambiguous "not the plaintext"
   *  signal. */
  get pending(): EnrollmentCodeCardProps[] {
    return this.tagmata
      .filter((t) => t.state === "pending")
      .map((t) => {
        const plaintext = this.mintedCode[t.tagma_id];
        const code = plaintext ?? t.code_masked ?? "";
        return {
          id: t.tagma_id,
          label: t.label,
          createdAt: t.created_at,
          expiresAt: t.expires_at ?? "",
          code,
          copyable: plaintext !== undefined,
        };
      });
  }

  /** Enrolled tagmata as card props WITHOUT presence. The registry owns
   * identity/label/createdAt only; live presence is overlaid by the view from
   * realtime (the archeion `/v1/lesche/tagmata` no longer carries liveness). */
  get enrolledCards(): Omit<TagmaCardProps, "presence">[] {
    return this.tagmata
      .filter((t) => t.state === "enrolled")
      .map((t) => ({
        tagmaId: t.tagma_id,
        label: t.label,
        createdAt: t.created_at,
      }));
  }

  /**
   * Resolve the signed-in user. A 401/403 means "no session" (logged out) ->
   * `user = null`. Any other failure (500, network) is transient: leave `user`
   * at `undefined` and surface the error, so guards render a skeleton instead
   * of booting the user to /login on a backend hiccup.
   */
  async whoami(): Promise<void> {
    try {
      this.user = await client().me();
      this.participantId = await participantIdForUser(this.user.user_id);
      this.authError = null;
    } catch (e) {
      if (
        e instanceof ArcheionApiError &&
        (e.status === 401 || e.status === 403)
      ) {
        this.user = null;
        this.participantId = null;
        this.authError = null;
      } else {
        console.error("[archeion] whoami failed:", e);
        this.authError = auth_couldnt_reach();
      }
    }
  }

  /** Run the registration ceremony; on success resolve the profile. */
  async register(args: {
    username: string;
    display_name?: string;
  }): Promise<CeremonyResult> {
    const result = await registerWithPasskey(client(), args);
    this.lastCeremony = result;
    if (result.ok) {
      markViaPasskey();
      await this.whoami();
    }
    return result;
  }

  /** Run the login ceremony (username is the login id); on success resolve the profile. */
  async login(username: string): Promise<CeremonyResult> {
    const result = await loginWithPasskey(client(), username);
    this.lastCeremony = result;
    if (result.ok) {
      markViaPasskey();
      await this.whoami();
    }
    return result;
  }
  /** Operator-key login (local-platform deployments): exchange the sk-admin-
   * key for a session, then park it in sessionStorage so the instances
   * client's bearer header reuses it (token double-use; cleared on logout).
   * Failures are typed statuses the login page maps to copy; transport
   * errors still throw.
   */
  async adminLogin(key: string): Promise<AdminLoginResult> {
    const result = await adminLoginWithKey(client(), key);
    if (result.ok) {
      sessionStorage.setItem(INSTANCES_TOKEN_KEY, key);
      clearViaPasskey();
      await this.whoami();
    }
    return result;
  }

  /** Run the discoverable (usernameless) login ceremony via conditional-UI
   * autofill. Resolves non-ok (typically `cancelled`) on unsupported browsers
   * or when no discoverable credential matches; the caller falls back to
   * `login`. On success resolves the profile. `signal` lets the caller abort a
   * pending conditional get before starting an explicit ceremony. */
  async loginDiscoverable(signal?: AbortSignal): Promise<CeremonyResult> {
    const result = await loginWithDiscoverablePasskey(client(), signal);
    this.lastCeremony = result;
    if (result.ok) {
      markViaPasskey();
      await this.whoami();
    }
    return result;
  }

  async logout(): Promise<void> {
    try {
      await client().logout();
    } catch {
      // Even a failed logout clear should drop the local session.
    }
    this.reset();
  }

  /** Fetch the owner's tagmata (pending + enrolled). */
  async refreshTagmata(): Promise<void> {
    this.tagmataError = null;
    try {
      // The once-shown plaintext does not survive a refresh: the archeion returns
      // only the masked form, and the just-minted cards drop their plaintext.
      this.mintedCode = {};
      this.tagmata = await client().listTagmata();
      this.tagmataLoaded = true;
    } catch (e) {
      // Leave the stale list + loaded flag so a refresh failure does not blank it.
      console.error("[archeion] listTagmata failed:", e);
      this.tagmataError = rooms_couldnt_reach();
    }
  }

  /**
   * Set or clear a tagma's label (pending or enrolled). On success mirrors the
   * new label into the local list; on error it THROWS (the card surfaces it
   * inline). Deliberately does not touch `tagmataError`: that field blanks the
   * whole section, and a single failed rename must not do that.
   */
  async renameTagma(id: string, label: string | null): Promise<void> {
    await client().renameTagma(id, label);
    const resolved = label && label.trim() ? label.trim() : null;
    this.tagmata = this.tagmata.map((t) =>
      t.tagma_id === id ? { ...t, label: resolved } : t,
    );
  }

  /**
   * Mint a new pending tagma (enrollment code); the plaintext is shown once on
   * the new card. Prepend so the freshly-minted card is on top.
   */
  async mintTagma(): Promise<{ id: string; code: string } | null> {
    this.minting = true;
    try {
      const minted = await client().mintTagma();
      this.mintedCode = { ...this.mintedCode, [minted.id]: minted.code };
      this.tagmata = [
        {
          tagma_id: minted.id,
          label: null,
          state: "pending" as const,
          created_at: minted.created_at,
          // No masked form for a just-minted card; the plaintext (in
          // `mintedCode`) is shown until the next refresh.
        },
        ...this.tagmata,
      ];
      this.tagmataLoaded = true;
      this.tagmataError = null;
      return { id: minted.id, code: minted.code };
    } catch (e) {
      console.error("[archeion] mintTagma failed:", e);
      this.tagmataError = rooms_couldnt_reach();
      return null;
    } finally {
      this.minting = false;
    }
  }

  /**
   * Revoke a tagma (pending or enrolled); on success drop it from the list. For
   * an enrolled tagma the archeion cuts the tagma off on its next request. On
   * error it THROWS (the caller -- the card / dialog -- surfaces it inline),
   * mirroring `renameTagma`: a single failed revoke must not blank the whole
   * dashboard the way a `tagmataError` would.
   */
  async revokeTagma(id: string): Promise<void> {
    await client().revokeTagma(id);
    this.tagmata = this.tagmata.filter((t) => t.tagma_id !== id);
    const next = { ...this.mintedCode };
    delete next[id];
    this.mintedCode = next;
  }

  // -- passkeys (self-service device management) ---------------------------

  /** Fetch the signed-in user's live passkeys. */
  async refreshPasskeys(): Promise<void> {
    this.passkeysError = null;
    try {
      this.passkeys = await client().listPasskeys();
      this.passkeysLoaded = true;
    } catch (e) {
      // Leave the stale list + loaded flag so a refresh failure does not blank it.
      console.error("[archeion] listPasskeys failed:", e);
      this.passkeysError = auth_couldnt_reach();
    }
  }

  /**
   * Bind another passkey to the signed-in account (a second-device ceremony).
   * The driver handles the one-shot step-up (re-login) internally; the typed
   * result is stashed in `lastAddPasskey` for the page. On success the new
   * passkey is prepended to the local list.
   */
  async addPasskey(
    label: string,
    opts: { discoverable?: boolean } = {},
  ): Promise<AddPasskeyResult> {
    if (!this.user) {
      const result: AddPasskeyResult = {
        ok: false,
        reason: "unknown",
        message: auth_no_signed_in_user(),
      };
      this.lastAddPasskey = result;
      return result;
    }
    // Credential-agnostic step-up: pass the username only when the account has
    // a passkey (so the driver can re-auth via passkey). An OAuth-only account
    // omits it -> the driver returns reauth-required -> the UI re-establishes
    // the step-up via OAuth (which navigates away and back).
    const hasPasskey = (this.user.passkey_count ?? 0) > 0;
    const result = await addPasskey(client(), {
      label,
      ...(hasPasskey ? { username: this.user.username } : {}),
      ...(opts.discoverable ? { discoverable: true } : {}),
    });
    this.lastAddPasskey = result;
    if (result.ok) {
      this.passkeys = [result.passkey, ...this.passkeys];
      this.passkeysError = null;
    }
    return result;
  }

  /** Rename a passkey; mirrors `renameTagma` (locally patches, THROWS on error). */
  async renamePasskey(id: string, label: string): Promise<void> {
    const updated = await client().renamePasskey(id, label);
    this.passkeys = this.passkeys.map((p) => (p.id === id ? updated : p));
  }

  /** Revoke a passkey (hard-delete + audit); mirrors `revokeTagma` (locally
   *  removes, THROWS on error). The archeion refuses the last live passkey (409). */
  async revokePasskey(id: string): Promise<void> {
    await client().revokePasskey(id);
    this.passkeys = this.passkeys.filter((p) => p.id !== id);
  }

  // -- oauth (provider sign-in + linked identities) ------------------------

  /** Fetch the enabled OAuth providers (unauthenticated). Best-effort: a
   *  failure leaves the list empty (no provider buttons rendered). The loaded
   *  flag is set ONLY on success: a transient failure leaves it false so the
   *  settings effect (which guards on the flag) refetches on the next mount,
   *  recovering a /login-time blip rather than hiding providers for the whole
   *  session. This cannot loop: the effect's tracked deps are the flag + `user`,
   *  not the providers array, so a failed fetch (which reassigns the array but
   *  not the flag) does not retrigger the effect. */
  async refreshOAuthProviders(): Promise<void> {
    try {
      this.oauthProviders = await client().listOAuthProviders();
      this.oauthProvidersLoaded = true;
    } catch {
      this.oauthProviders = [];
    }
  }

  /** Refresh the signed-in user's linked OAuth identities (mirrors the passkeys
   *  error discipline: a fetch failure lands in `externalIdentitiesError`). */
  async refreshExternalIdentities(): Promise<void> {
    this.externalIdentitiesError = null;
    try {
      this.externalIdentities = [...(await client().listExternalIdentities())];
      this.externalIdentitiesLoaded = true;
    } catch (e) {
      console.error("[archeion] listExternalIdentities failed:", e);
      this.externalIdentitiesError = auth_couldnt_reach();
    }
  }

  /** Refresh just the signed-in user's emails slice (`GET /me/emails`) and patch
   *  it onto `user` in place, re-deriving `primary_email` from the list. Cheaper
   *  than a full `whoami()` (which refetches the whole `/me`), so the email
   *  settings mutations each cost one narrow round-trip. THROWS on failure; the
   *  caller (EmailManager) surfaces it. */
  async refreshEmails(): Promise<void> {
    if (!this.user) return;
    const list = await client().listEmails();
    // Reassign the whole user: MeResponse fields are readonly, so the slice is
    // patched via a spread (this also notifies the $state).
    this.user = {
      ...this.user,
      emails: list,
      primary_email: list.find((e) => e.is_primary)?.address ?? null,
    };
  }

  // -- provider vault (self-service API-key storage) -----------------------

  /** Fetch the signed-in user's provider vault. Mirrors the passkeys error
   *  discipline: a list failure lands in `providersError`, never blanking `user`. */
  async refreshProviders(): Promise<void> {
    this.providersError = null;
    try {
      this.providers = await client().listProviders();
      this.providersLoaded = true;
    } catch (e) {
      console.error("[archeion] listProviders failed:", e);
      this.providersError = auth_couldnt_reach();
    }
  }

  /** Create a vault entry; mirrors `renamePasskey` (local insert, THROWS). A
   *  duplicate name is a 409 the caller surfaces inline. An `encrypted`
   *  request is sealed with the device vault key HERE, so a plaintext key
   *  never leaves this browser unencrypted and callers stay unaware of the
   *  ceremony (the same policy `flipProviderEncryption` applies). */
  async createProvider(body: ProviderRequest): Promise<ProviderSummary> {
    const sealed =
      body.mode === "encrypted"
        ? {
            ...body,
            key_material: await seal(
              await deviceVault.ensure(),
              body.key_material,
            ),
          }
        : body;
    const created = await client().createProvider(sealed);
    this.providers = [...this.providers, created];
    return created;
  }

  /** Replace a vault entry in place (rename / key rotation / mode flip).
   *  key_material is stored exactly as sent: unlike create there is no
   *  built-in sealing here, so an encrypted row must pass its existing blob
   *  back or a fresh seal() output, never a raw key. THROWS on error like
   *  every other mutation. */
  async replaceProvider(id: string, body: ProviderRequest): Promise<void> {
    const updated = await client().replaceProvider(id, body);
    this.providers = this.providers.map((p) => (p.id === id ? updated : p));
  }

  /** Delete a vault entry; THROWS on error. The archeion returns the remaining
   *  list, which becomes the new local state (mirrors removeEmail). */
  async deleteProvider(id: string): Promise<void> {
    this.providers = await client().deleteProvider(id);
  }

  /** Whether THIS login arrived via a passkey ceremony; gates the vault's
   *  encryption affordances (see VIA_PASSKEY_KEY doc -- a UI hint, not a
   *  security boundary). Not reactive by design: it changes only across
   *  logins, which re-render the session anyway. */
  canFlipKeys(): boolean {
    return viaPasskey();
  }

  /** Rename a vault entry in place: replace resends every field unchanged
   *  except the name. THROWS on error like every other mutation (a duplicate
   *  name is a 409 the caller surfaces inline). */
  async renameProvider(id: string, name: string): Promise<void> {
    const current = this.providers.find((p) => p.id === id);
    if (!current) throw new Error("provider not in local list");
    await this.replaceProvider(id, {
      name,
      provider: current.provider,
      base_url: current.base_url,
      key_material: current.key_material,
      mode: current.mode,
    });
  }

  /** Flip one entry between plaintext and encrypted storage IN THIS BROWSER:
   *  seal with the device vault key or open the blob locally, then send the
   *  new form. The flip affordance is gated to passkey-arrived sessions
   *  upstack; the ceremony itself only needs the device key. THROWS when an
   *  encrypted blob cannot be opened with THIS device's key. */
  async flipProviderEncryption(entry: ProviderSummary): Promise<void> {
    const key = await deviceVault.ensure();
    if (entry.mode === "plaintext") {
      await this.replaceProvider(entry.id, {
        name: entry.name,
        provider: entry.provider,
        base_url: entry.base_url,
        key_material: await seal(key, entry.key_material),
        mode: "encrypted",
      });
    } else {
      await this.replaceProvider(entry.id, {
        name: entry.name,
        provider: entry.provider,
        base_url: entry.base_url,
        key_material: await open(key, entry.key_material),
        mode: "plaintext",
      });
    }
  }

  /** Return the usable key material for copying: plaintext entries pass
   *  through; encrypted blobs are opened with THIS device's key, returning
   *  null when that fails (expected for rows sealed on another device). */
  async revealProviderKey(entry: ProviderSummary): Promise<string | null> {
    if (entry.mode === "plaintext") return entry.key_material;
    try {
      const key = await deviceVault.ensure();
      return await open(key, entry.key_material);
    } catch (e) {
      console.error("[vault] open failed:", e);
      return null;
    }
  }

  /** Start an OAuth SIGN-IN from the login/register page: navigate to the
   *  provider. No return value -- the SPA unloads; the callback page completes
   *  the ceremony. `returnPath` is the sanitized path to resume to after
   *  sign-in. Stashes the callback context AFTER the begin succeeds (not
   *  before) so a failed begin does not leave a stale context in sessionStorage.
   *  Calls the low-level client method directly rather than the
   *  `signInWithOAuth` convenience (which navigates itself); we need to stash
   *  between the begin resolving and the page unloading. */
  async signInWithOAuth(provider: string, returnPath?: string): Promise<void> {
    const { authorize_url } = await client().oauthSignInBegin(
      provider,
      returnPath ? { return_path: returnPath } : {},
    );
    stashOAuthCallback({ provider, action: "signin" });
    window.location.href = authorize_url;
  }

  /** Start an OAuth LINK from settings (step-up gated server-side): navigate to
   *  the provider. The callback page completes the link and the user returns to
   *  settings. Stashes the callback context AFTER the begin succeeds so a failed
   *  begin (e.g. step-up stale, archeion unreachable) does not leave a stale
   *  context in sessionStorage. */
  async linkProvider(provider: string): Promise<void> {
    const { authorize_url } = await client().oauthLinkBegin(provider, {});
    stashOAuthCallback({ provider, action: "link" });
    window.location.href = authorize_url;
  }

  /** Unlink an OAuth identity (hard-delete). THROWS on the 409 last-method
   *  guard (the caller surfaces it). */
  async unlinkExternalIdentity(id: string): Promise<void> {
    await client().unlinkExternalIdentity(id);
    this.externalIdentities = this.externalIdentities.filter(
      (e) => e.id !== id,
    );
  }

  /** Complete an OAuth ceremony on the callback page: exchange `code`+`state`.
   *  On a signin the archeion sets the session cookie, so `whoami()` is re-run
   *  before the caller navigates. Returns the typed result so the callback page
   *  can branch (signin resumes to `returnPath`/home; link returns to settings). */
  async completeOAuth(
    provider: string,
    body: { state: string; code: string },
  ): Promise<OAuthCompleteResult> {
    const result = await completeOAuth(client(), provider, body);
    if (result.ok && result.kind === "signin") {
      clearViaPasskey();
      await this.whoami();
      // The finish XHR set the session cookie; a transient whoami blip (a
      // network race right after the cookie landed) should not strand the
      // just-signed-in user unresolved. Retry once.
      if (!this.user) await this.whoami();
    } else if (result.ok && result.kind === "link") {
      await this.refreshExternalIdentities();
    }
    return result;
  }

  /** Submit the chosen username for a held OAuth signup (the token came from a
   *  needs-username finish). On success the complete XHR set the session cookie,
   *  so `whoami()` is re-run before the caller navigates. */
  async completeOAuthSignup(body: {
    signupToken: string;
    username: string;
  }): Promise<OAuthSignupResult> {
    const result = await completeOAuthSignup(client(), body);
    if (result.ok) {
      await this.whoami();
      if (!this.user) await this.whoami();
    }
    return result;
  }

  /** Complete an OAuth ceremony on the callback page: read the stashed provider
   *  (the redirect carries only `code`+`state`), exchange them, and refresh the
   *  relevant session slice. Returns the typed result so the page navigates by
   *  `kind` (signin -> home/returnPath, link -> settings). */
  async completeOAuthFromCallback(
    code: string,
    state: string,
  ): Promise<OAuthCompleteResult> {
    let ctx: OAuthCallbackContext | null = null;
    try {
      ctx = JSON.parse(
        sessionStorage.getItem(OAUTH_CALLBACK_KEY) ?? "null",
      ) as OAuthCallbackContext | null;
      sessionStorage.removeItem(OAUTH_CALLBACK_KEY);
    } catch {
      ctx = null;
    }
    if (!ctx) {
      return {
        ok: false,
        reason: "unknown",
        message: oauth_context_lost(),
      };
    }
    return this.completeOAuth(ctx.provider, { state, code });
  }

  // -- device pairing (cross-device enrollment) ----------------------------

  /**
   * Mint a short-lived pairing code on THIS signed-in device, to be
   * redeemed by a new device. Step-up-gated like addPasskey: a 403
   * `reauth-required` triggers a re-login then a single retry. The plaintext
   * code is stashed in `pairingCode` (shown once); only its hash is retained
   * server-side. Returns true on success (the page clears its mint button).
   */
  async mintPairingCode(): Promise<boolean> {
    const username = this.user?.username;
    const hasPasskey = (this.user?.passkey_count ?? 0) > 0;
    if (!username) {
      this.pairingError = auth_no_signed_in_user();
      return false;
    }
    this.pairingError = null;
    // One step-up retry on reauth-required (mirrors addPasskey).
    let resp;
    try {
      resp = await client().mintPairingCode();
    } catch (e) {
      if (!(e instanceof ArcheionApiError) || e.status !== 403) {
        console.error("[archeion] mintPairingCode failed:", e);
        this.pairingError = auth_couldnt_reach();
        return false;
      }
      // Credential-agnostic step-up: only an account WITH a passkey can re-auth
      // via passkey. An OAuth-only account has no passkey to assert, so a
      // passkey step-up would dead-end on a ceremony it cannot complete --
      // surface a clear re-auth prompt instead. (The in-page OAuth retry that
      // refreshes authed_at is a documented follow-up.)
      if (!hasPasskey) {
        this.pairingError = auth_reauth_required();
        return false;
      }
      const login = await loginWithPasskey(client(), username);
      if (!login.ok) {
        // Surface the step-up login's own failure (e.g. a bad assertion), not
        // the original 403 body that triggered the re-auth.
        this.pairingError =
          login.reason === "cancelled"
            ? auth_passkey_cancelled()
            : (login.message ?? pair_reauth_failed());
        return false;
      }
      try {
        resp = await client().mintPairingCode();
      } catch (e2) {
        console.error("[archeion] mintPairingCode (step-up retry) failed:", e2);
        this.pairingError = auth_couldnt_reach();
        return false;
      }
    }
    this.pairingCode = { code: resp.code, expiresAt: resp.expires_at };
    return true;
  }

  /**
   * Pair THIS (anonymous) device onto an existing account using a code minted
   * by an already-signed-in device. Runs the ceremony and resolves the profile
   * on success (this device is then signed in). The result is stashed in
   * `lastPair` for the page to render.
   */
  async pairDevice(code: string, label: string): Promise<PairResult> {
    const result = await pairDevice(client(), { code, label });
    this.lastPair = result;
    if (result.ok) {
      markViaPasskey();
      await this.whoami();
    }
    return result;
  }
  /** Copy a just-minted secret to the clipboard and flash the card's "Copied". */
  async copySecret(id: string, secret: string): Promise<void> {
    if (!(await copyText(secret))) return;
    this.copiedCodeId = id;
    setTimeout(() => {
      if (this.copiedCodeId === id) this.copiedCodeId = null;
    }, 2000);
  }

  /** Drop all local state (logout). */
  private reset(): void {
    this.user = null;
    this.participantId = null;
    this.tagmata = [];
    this.tagmataLoaded = false;
    this.mintedCode = {};
    this.authError = null;
    this.tagmataError = null;
    this.lastCeremony = null;
    this.copiedCodeId = null;
    this.minting = false;
    this.passkeys = [];
    this.passkeysError = null;
    this.passkeysLoaded = false;
    this.lastAddPasskey = null;
    this.externalIdentities = [];
    this.externalIdentitiesError = null;
    this.externalIdentitiesLoaded = false;
    this.oauthProviders = [];
    this.oauthProvidersLoaded = false;
    this.providers = [];
    this.providersError = null;
    this.providersLoaded = false;
    this.pairingCode = null;
    this.pairingError = null;
    this.lastPair = null;
    clearViaPasskey();
    sessionStorage.removeItem(INSTANCES_TOKEN_KEY);
  }
}

export const archeionSession = new ArcheionSessionStore();
