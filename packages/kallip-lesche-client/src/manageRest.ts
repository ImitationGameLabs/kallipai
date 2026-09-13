// Manage-plane REST client for the lesche reverse proxy:
// `/v1/tagmata/{id}/manage/{*path}`. Unlike the envelope path, manage
// metadata is plaintext by design (TLS + device-proof tunnel auth is the
// trust base), so this client speaks plain credentialed HTTP and passes
// the response status + body through untouched -- the caller decides how
// to interpret non-2xx shapes (proxy text errors vs tagma ApiError JSON).

/** CSRF marker the lesche's `csrf_guard` checks on mutating requests. */
const CSRF_HEADER = "X-Requested-With";
const CSRF_HEADER_VALUE = "kallip";

export class ManageRestClient {
  constructor(private readonly baseUrl: string) {}

  /**

* Run one manage op against a tagma through the reverse proxy. The proxy
* strips query strings (the one sanctioned filter, /agents?include=,
* rides the frame body) and enforces the frame allowlist upstream, so a
* 404 here can mean "not on the manage frame surface".
*
* Pass-through contract: resolves with the raw status and parsed body for
* ANY status, never throws for non-2xx -- the caller maps error shapes.
  */
  async manage(
    agent: string,
    method: string,
    path: string,
    body?: unknown,
  ): Promise<{ status: number; body: unknown }> {
    const headers: Record<string, string> = { accept: "application/json" };
    if (method !== "GET") {
      headers[CSRF_HEADER] = CSRF_HEADER_VALUE;
      if (body !== undefined) headers["content-type"] = "application/json";
    }
    const resp = await fetch(
      `${this.baseUrl}/tagmata/${encodeURIComponent(agent)}/manage${path}`,
      {
        method,
        headers,
        credentials: "include",
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      },
    );
    const text = await resp.text();
    let parsed: unknown = null;
    if (text.length > 0) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = text; // proxy text errors (e.g. plain "not your tagma")
      }
    }
    return { status: resp.status, body: parsed };
  }
}
