// AgoraSessionStore: reactive ($state) wrapper around the agora client holding
// the signed-in user and the owner's tagmata across their lifecycle (pending ->
// enrolled -> revoked).
//
// Error discipline mirrors SessionStore (session.svelte.ts): auth-fatal errors
// (whoami/register/login/logout) live in `authError` and clear `user`; the list
// error lives in `tagmataError` so a fetch failure never blanks the signed-in
// state or vice-versa. `user` is tri-state: `undefined` = unresolved (the root
// layout is still calling whoami), `null` = resolved logged-out, `MeResponse` =
// signed in. The auth gate treats only `null` as "redirect to /login", so a
// transient network failure (user stays undefined) renders a skeleton rather
// than booting the user out.
//
// The agora base URL is injected via initAgora() at app bootstrap -- this
// package does not read import.meta.env (which is only typed in a SvelteKit
// app, not a library).

import {
  AgoraApiError,
  AgoraClient,
  type CeremonyResult,
  LescheClient,
  loginWithPasskey,
  type MeResponse,
  registerWithPasskey,
  type TagmaView,
} from "@kallipai/kallip-agora-client";
import type {
  EnrollmentCodeCardProps,
  TagmaCardProps,
} from "../tagmata.svelte.ts";

let agoraClient: AgoraClient | null = null;

/** Inject the agora base URL and construct the client. Called once at bootstrap. */
export function initAgora(url: string): void {
  agoraClient = new AgoraClient(url);
}

function client(): AgoraClient {
  if (!agoraClient) {
    throw new Error("initAgora(url) must be called at app bootstrap");
  }
  return agoraClient;
}

/** The control-plane (agora) client; throws if initAgora has not been called.
 * Exposed so peer stores (e.g. the channels store, which needs `getTagma` for
 * the key-exchange's pinned key) can reach the same singleton. */
export function agoraClientOrFail(): AgoraClient {
  return client();
}

// The lesche (data-plane) client lives on a separate origin from the agora; its
// URL is injected the same way (no import.meta.env in this library). The session
// cookie is shared cross-subdomain, so the same credentialed fetch works.
let lescheClient: LescheClient | null = null;

/** Inject the lesche base URL and construct the data-plane client. Called once
 * at bootstrap alongside initAgora. */
export function initLesche(url: string): void {
  lescheClient = new LescheClient(url);
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

function messageOf(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}

class AgoraSessionStore {
  // Tri-state: undefined = unresolved, null = logged out, MeResponse = signed in.
  //
  // Invariant: this field is only meaningful in online mode. The agora session
  // cookie survives offline mode (we never logout() on a mode switch), so `user`
  // can remain a stale MeResponse while the app is in offline mode. Offline UI
  // must not branch on it -- the status snippet, nav, and gate are all
  // mode-gated, so nothing in offline mode reads `user`. Do not change that
  // without adding a guard.
  user: MeResponse | null | undefined = $state(undefined);

  // Split errors (see file comment).
  authError: string | null = $state(null);
  tagmataError: string | null = $state(null);

  // Raw ceremony result for the auth pages to render inline.
  lastCeremony: CeremonyResult | null = $state(null);

  // The owner's tagmata (pending + enrolled; revoked are never listed), newest
  // first. The agora owns code masking; this store holds no separate secret
  // cache beyond the transient `mintedCode` (the once-shown plaintext).
  tagmata: TagmaView[] = $state([]);
  tagmataLoaded = $state(false);

  minting = $state(false);
  copiedCodeId: string | null = $state(null);

  // The plaintext of just-minted pending tagmas, shown once on the new card
  // (transient -- dropped on the next refresh, when the agora's masked value
  // takes over). Keyed by tagma id.
  private mintedCode: Record<string, string> = {};

  /** Pending tagmata as card props. `code` is the just-minted full plaintext
   *  while `mintedCode` holds it (the only chance to copy); otherwise the agora's
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
   * realtime (the agora `/v1/tagmata` no longer carries liveness). */
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
      this.authError = null;
    } catch (e) {
      if (
        e instanceof AgoraApiError &&
        (e.status === 401 || e.status === 403)
      ) {
        this.user = null;
        this.authError = null;
      } else {
        this.authError = messageOf(e);
      }
    }
  }

  /** Run the registration ceremony; on success resolve the profile. */
  async register(args: {
    invite_code: string;
    email: string;
    username: string;
    display_name?: string;
  }): Promise<CeremonyResult> {
    const result = await registerWithPasskey(client(), args);
    this.lastCeremony = result;
    if (result.ok) await this.whoami();
    return result;
  }

  /** Run the login ceremony (email is the login id); on success resolve the profile. */
  async login(email: string): Promise<CeremonyResult> {
    const result = await loginWithPasskey(client(), email);
    this.lastCeremony = result;
    if (result.ok) await this.whoami();
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
      // The once-shown plaintext does not survive a refresh: the agora returns
      // only the masked form, and the just-minted cards drop their plaintext.
      this.mintedCode = {};
      this.tagmata = await client().listTagmata();
      this.tagmataLoaded = true;
    } catch (e) {
      // Leave the stale list + loaded flag so a refresh failure does not blank it.
      this.tagmataError = messageOf(e);
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
  async mintTagma(): Promise<void> {
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
    } catch (e) {
      this.tagmataError = messageOf(e);
    } finally {
      this.minting = false;
    }
  }

  /**
   * Revoke a tagma (pending or enrolled); on success drop it from the list. For
   * an enrolled tagma the agora cuts the herald off on its next request. On
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

  /** Copy a just-minted secret to the clipboard and flash the card's "Copied". */
  async copySecret(id: string, secret: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(secret);
      this.copiedCodeId = id;
      setTimeout(() => {
        if (this.copiedCodeId === id) this.copiedCodeId = null;
      }, 2000);
    } catch {
      // Clipboard may be unavailable (permissions, non-secure context); ignore.
    }
  }

  /** Drop all local state (logout). */
  private reset(): void {
    this.user = null;
    this.tagmata = [];
    this.tagmataLoaded = false;
    this.mintedCode = {};
    this.authError = null;
    this.tagmataError = null;
    this.lastCeremony = null;
    this.copiedCodeId = null;
    this.minting = false;
  }
}

export const agoraSession = new AgoraSessionStore();
