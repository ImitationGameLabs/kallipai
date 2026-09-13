// Client for the local instances service (kallip-instances): the offline
// home reads machine-level instance state through its /api/instances/* HTTP
// face. Deliberately separate from manage/backend.ts — the instances
// service is a third backend with its own uniform {code, message} error
// shape, and instance lifecycle is not a tagma management concern.
/** sessionStorage key holding the standalone-mode bearer token; when set,
 * every request carries it as the authorization header.
 */
export const INSTANCES_TOKEN_KEY = "kallip:instances-token";

/** One-shot handoff key: the spawn form parks the new instance's operator
 * token here when the user provided one, and /connect picks it up exactly
 * once (read + remove) so the token never rides the URL.
 */
export const CONNECT_TOKEN_KEY = "kallip:connect-token";

/** One managed instance as the instances service reports it (wire mirror).
 */
export interface InstanceInfo {
  slug: string;
  instance_id: string;
  workspace: string;
  running: boolean;
  /** The instance's liveness state as the daemon classifies it.
   * "unknown" arrives only on a version-skewed pair (the daemon spoke a
   * state token this UI never wrote); liveness decisions key on
   * `running`, which stays truthful. */
  state: "running" | "stopped" | "dead" | "unknown";
  /** The instance's current listen port (from runtime.json via the wire)
   * when running; null/absent when stopped or the daemon predates it. */
  port?: number | null;
  owner: number | null;
  /** The enrolled tagma identity (archeion-issued), read by the daemon scan
   * from the instance's own `credentials/<entry>/tagma.id`. Absent when the
   * instance never enrolled, the read failed, or the daemon predates the
   * field — the panel join then falls back to the slug convention. */
  tagma_id?: string | null;
}

/** Liveness report: the daemon itself, or one instance by slug.
 */
export interface InstanceHealth {
  slug: string | null;
  running: boolean;
  /** Live states: see InstanceInfo.state. */
  state: "running" | "stopped" | "dead" | "unknown";
  detail: string | null;
}

/** Spawn request: the daemon's allowlisted env pairs ride as KEY=VALUE.
 */
export interface InstanceSpawnInput {
  slug: string;
  workspace: string;
  env: string[];
}

/** One freshly launched instance (the proxy unwraps the wire payload).
 */
export interface InstanceSpawnResult {
  slug: string;
  pid: number;
  port: number;
}

/** The error kinds the offline instances page branches on.
 */
export type InstancesErrorKind =
  | "unauthorized"
  | "forbidden"
  | "unreachable"
  | "other";

/** One failed exchange, classified to the kind the page renders.
 */
export class InstancesError extends Error {
  constructor(
    readonly kind: InstancesErrorKind,
    message: string,
    readonly status?: number,
    readonly code?: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
  }
}

export class InstancesClient {
  readonly base: string;

  constructor(baseUrl: string) {
    this.base = baseUrl.replace(/\/+$/, "");
  }

  /** The daemon's own liveness (no slug).
   */
  health(): Promise<InstanceHealth> {
    return this.get<InstanceHealth>("/health");
  }

  /** Every instance the daemon sees, with its running bit.
   */
  async list(): Promise<InstanceInfo[]> {
    const body = await this.get<{ instances: InstanceInfo[] }>("/list");
    return body.instances;
  }
  /** Provisioning methods the backend advertises (empty = the create
   * entry hides, the page stays).
   */
  async capabilities(): Promise<string[]> {
    const body = await this.get<{ methods: string[] }>("/capabilities");
    return body.methods;
  }

  /** Launch one instance; the response carries its listen port.
   */
  spawn(input: InstanceSpawnInput): Promise<InstanceSpawnResult> {
    return this.post<InstanceSpawnResult>("/spawn", input);
  }

  /** Stop one instance by slug.
   */
  stop(slug: string): Promise<{ slug: string }> {
    return this.post<{ slug: string }>("/stop", { slug });
  }

  /** Relaunch a stopped or dead instance from its persisted tree.
   */
  start(slug: string): Promise<{ slug: string; pid: number; port: number }> {
    return this.post<{ slug: string; pid: number; port: number }>("/start", {
      slug,
    });
  }

  private async request<T>(path: string, init?: RequestInit): Promise<T> {
    // The session cookie is the browser-channel credential (the admin
    // login carries instances rights); a stored bearer token still rides
    // along when present (app / manual entry).
    const credentials: RequestCredentials = "include";
    let response: Response;
    try {
      response = await fetch(this.base + path, {
        ...init,
        credentials,
        headers: { ...this.authHeaders(init?.method), ...init?.headers },
      });
    } catch (cause) {
      throw new InstancesError(
        "unreachable",
        "instances service unreachable: " + path,
        undefined,
        undefined,
        { cause },
      );
    }
    if (response.ok) {
      return (await response.json()) as T;
    }
    throw await this.fault(response);
  }

  private get<T>(path: string): Promise<T> {
    return this.request<T>(path);
  }

  private post<T>(path: string, body: unknown): Promise<T> {
    return this.request<T>(path, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  }

  /** Headers every exchange carries; a stored token rides along, and
   * non-GETs carry the CSRF marker the cookie channel requires (the
   * bearer channel is exempt; sending it anyway is harmless). */
  private authHeaders(method?: string): Record<string, string> {
    const headers: Record<string, string> = { accept: "application/json" };
    if (method !== undefined && method !== "GET") {
      headers["x-requested-with"] = "kallip";
    }
    const token = sessionStorage.getItem(INSTANCES_TOKEN_KEY);
    if (token) {
      headers.authorization = "Bearer " + token;
    }
    return headers;
  }

  /** Map one non-2xx response onto the page's branch kinds, keeping the
   * proxy's machine-readable code for message selection.
   */
  private async fault(response: Response): Promise<InstancesError> {
    const { code, message } = await faultBody(response);
    switch (response.status) {
      case 401:
        return new InstancesError(
          "unauthorized",
          message,
          response.status,
          code,
        );
      case 403:
        return new InstancesError("forbidden", message, response.status, code);
      case 502:
      case 503:
      case 504:
        return new InstancesError(
          "unreachable",
          message,
          response.status,
          code,
        );
      default:
        return new InstancesError("other", message, response.status, code);
    }
  }
}

/** The fault body's code and human message, or the status line when the
 * body isn't JSON.
 */
async function faultBody(
  response: Response,
): Promise<{ code?: string; message: string }> {
  try {
    const body = (await response.json()) as { code?: string; message?: string };
    return {
      code: body.code,
      message: body.message ?? "http " + String(response.status),
    };
  } catch {
    return { message: "http " + String(response.status) };
  }
}

// The service's base URL is injected via initInstances() at app bootstrap --
// the package does not read import.meta.env (SvelteKit-only typing). The
// value is the FULL service base (origin + /v1/instances, as served by
// the api.<domain> edge): the service nests its routes there.
let instancesClient: InstancesClient | null = null;

/** Inject the service base URL and construct the client. Called once at bootstrap. */
export function initInstances(url: string): void {
  instancesClient = new InstancesClient(url);
}

export function instancesClientOrFail(): InstancesClient {
  if (!instancesClient) {
    throw new Error("initInstances(url) must be called at app bootstrap");
  }
  return instancesClient;
}

/** The single source of the tagma->instance slug convention: one-click
 *  spawns name the instance `tagma-<first 8 id chars>`, and the devices list
 *  joins identities to processes by this same prefix. Keeping both sides on
 *  this function is what makes the join verifiable instead of a duplicated
 *  template that can silently drift apart. */
export function instanceSlugFor(tagmaId: string): string {
  return `tagma-${tagmaId.slice(0, 8)}`;
}
