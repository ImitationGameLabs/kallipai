// AgoraClient: a thin HTTP client over the agora `/v1` surface. Browser-first:
// every fetch carries `credentials: "include"` (the session cookie is the auth
// for user routes) and every non-GET carries the CSRF marker
// (`X-Requested-With: kallip`, matching `session.rs`), which the agora's
// `csrf_guard` requires on cookie-bearing mutating requests. Non-2xx responses
// become `AgoraApiError` (`{ status, message }`).

import { AgoraApiError } from "./types.ts";
import type {
  AuthFinishResponse,
  LoginBeginResponse,
  LoginFinishRequest,
  MeResponse,
  MintTagmaResponse,
  RegisterBeginResponse,
  RegisterFinishRequest,
  RenameTagmaRequest,
  TagmaView,
} from "./types.ts";

/** CSRF marker the agora's `csrf_guard` checks (see `session.rs:21-24`). */
export const CSRF_HEADER = "X-Requested-With";
export const CSRF_HEADER_VALUE = "kallip";

/** Request bodies for the ceremony begins. Mirrors the agora DTOs in
 * `crates/kallip-agora/src/routes/auth.rs` (`RegisterBeginRequest`,
 * `LoginBeginRequest`): email is the login id; username is the in-site handle. */
export interface RegisterBeginRequest {
  readonly invite_code: string;
  readonly email: string;
  readonly username: string;
  readonly display_name?: string;
}
export interface LoginBeginRequest {
  readonly email: string;
}

export class AgoraClient {
  constructor(private readonly baseUrl: string) {}

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

  logout(): Promise<void> {
    return this.json("/v1/auth/logout", "POST", undefined);
  }

  // -- profile --------------------------------------------------------------

  me(): Promise<MeResponse> {
    return this.json("/v1/me", "GET");
  }

  // -- tagmata (unified pending + enrolled lifecycle) -----------------------

  /** `POST /v1/tagmata` — mint a pending tagma (an enrollment code). The
   * plaintext `code` is returned once. */
  mintTagma(): Promise<MintTagmaResponse> {
    return this.json("/v1/tagmata", "POST", {});
  }

  /** `GET /v1/tagmata` — the caller's tagmata (pending + enrolled, not revoked),
   * newest first, each annotated with live presence. */
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
   * tagma the agora cuts the herald off on its next request. Returns on 204. */
  revokeTagma(id: string): Promise<void> {
    return this.json(
      `/v1/tagmata/${encodeURIComponent(id)}`,
      "DELETE",
      undefined,
    );
  }

  // -- internals ------------------------------------------------------------

  private async json<T>(
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
    if (!resp.ok) {
      // The agora's `ApiError` body is `{ status, message }`.
      let message = resp.statusText;
      try {
        const errorBody = (await resp.json()) as { message?: string };
        if (errorBody.message) message = errorBody.message;
      } catch {
        // Non-JSON error (e.g. a 403 from the CSRF guard); keep statusText.
      }
      throw new AgoraApiError(resp.status, message);
    }
    // 204 No Content (revoke) -- nothing to parse.
    if (resp.status === 204 || resp.headers.get("content-length") === "0") {
      return undefined as T;
    }
    return (await resp.json()) as T;
  }
}
