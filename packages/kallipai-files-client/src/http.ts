// Browser client plumbing for the files data plane. The files service
// authorizes bearer-first with a `kallipai_session` cookie fallback
// (kallipai-files/src/auth.rs), so the same credentialed fetch the other
// clients use works unchanged. Every fetch carries `credentials: "include"`
// (the session cookie is the auth) and every non-GET carries the CSRF marker
// (`X-Requested-With: kallipai`). Non-2xx responses become `FilesApiError`
// (`{ status, message }`), read from the common error envelope.

import { readApiError } from "@kallipai/kallipai-common";

/** A non-2xx files response, reduced to status + server message. */
export class FilesApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "FilesApiError";
    this.status = status;
  }
}

/** The CSRF marker the files middleware checks on cookie-borne mutations. */
export const CSRF_MARKER = "kallipai";

/** Base fetch with the shared session cookie and, for mutations, the CSRF
 * marker. Throws `FilesApiError` on any non-2xx. */
export async function filesFetch(
  base: string,
  path: string,
  init: RequestInit & { method: string },
): Promise<Response> {
  const headers = new Headers(init.headers);
  if (init.method !== "GET" && init.method !== "HEAD") {
    headers.set("X-Requested-With", CSRF_MARKER);
  }
  const response = await fetch(`${base}${path}`, {
    ...init,
    headers,
    credentials: "include",
  });
  if (!response.ok) {
    const { status, message } = await readApiError(response);
    throw new FilesApiError(status, message);
  }
  return response;
}
