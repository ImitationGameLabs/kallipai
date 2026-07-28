import {
  KallipError,
  TransportError,
  parseSseStream,
} from "@kallipai/kallip-common";
import type { AgentId } from "@kallipai/kallip-common";
import type {
  ExternalHistoryResponse,
  MessageResponse,
  WireAgentSummary,
} from "./types.ts";

export interface TagmaClientOptions {
  readonly baseUrl: string;
  readonly authToken?: string;
}

/**
 * Low-level HTTP client for the kallip tagma. TypeScript counterpart to the
 * Rust `kallip-client` crate's `TagmaClient`. Browser-first: uses `fetch` with
 * no Node globals. Throws {@link KallipError} on non-2xx (parsed from the
 * `{"error":{"message":...}}` envelope) and {@link TransportError} on network
 * failures.
 */
export class TagmaClient {
  private readonly base: string;
  private readonly token?: string;

  constructor(opts: TagmaClientOptions) {
    this.base = opts.baseUrl.replace(/\/+$/, "");
    this.token = opts.authToken;
  }

  private headers(extra?: Record<string, string>): Record<string, string> {
    const h: Record<string, string> = { ...extra };
    if (this.token) h["Authorization"] = `Bearer ${this.token}`;
    return h;
  }

  private async request(
    path: string,
    init: RequestInit = {},
  ): Promise<Response> {
    let resp: Response;
    try {
      resp = await fetch(this.base + path, {
        ...init,
        headers: this.headers(
          init.headers as Record<string, string> | undefined,
        ),
      });
    } catch (cause) {
      throw new TransportError(`tagma request failed: ${path}`, { cause });
    }
    if (!resp.ok) {
      throw new KallipError({
        status: resp.status,
        message: await readErrorMessage(resp),
      });
    }
    return resp;
  }

  private json<T>(path: string, init: RequestInit = {}): Promise<T> {
    return this.request(path, {
      ...init,
      headers: {
        "content-type": "application/json",
        ...(init.headers as Record<string, string> | undefined),
      },
    }).then((r) => r.json() as Promise<T>);
  }

  // --- agent surface ---

  postMessage(id: AgentId, text: string): Promise<MessageResponse> {
    return this.json<MessageResponse>(`/agents/${id}/message`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
  }

  /** Fetch the tagma's single root agent (always present after tagma startup). */
  getRootAgent(): Promise<WireAgentSummary> {
    return this.json<WireAgentSummary>("/agents/root");
  }

  // --- streaming events ---

  /** Subscribe to the agent's external event stream (the chat-room API): one
   * multiplexed SSE discriminated by the `event:` field ("authored" | "signal"
   * | "status"). Yields raw `{ event, data }` frames; the caller decodes each
   * payload per its event name. This is the frontend's sole window onto the
   * tagma -- authored assistant messages, runtime signals, and status
   * snapshots all arrive here. */
  async *externalEventStream(
    id: AgentId,
    signal?: AbortSignal,
  ): AsyncGenerator<{ readonly event: string; readonly data: string }> {
    const resp = await this.request(`/agents/${id}/external/events`, {
      method: "GET",
      signal,
    });
    const contentType = resp.headers.get("content-type") ?? "";
    if (!contentType.includes("text/event-stream")) {
      throw new TransportError(
        `expected text/event-stream, got ${contentType}`,
      );
    }
    for await (const raw of parseSseStream(resp, signal)) {
      // Keepalive / comment frames carry no `event:` name; skip them. Every
      // real frame on this stream is discriminated by its event name.
      if (!raw.event) continue;
      yield { event: raw.event, data: raw.data };
    }
  }

  /** Pull a cursor-driven history window for the direct (offline) path. The
   * direct SSE is live-only, so the frontend asks for back-log here using its
   * `maxRendered` high-water mark — symmetric with the relay's
   * `TagmaControl::History`. Omit `after`/`before` for the most recent `limit`
   * (a first-time device with an empty cache). */
  externalHistory(
    id: AgentId,
    opts: { after?: number | null; before?: number | null; limit?: number },
  ): Promise<ExternalHistoryResponse> {
    const params = new URLSearchParams();
    if (opts.after != null) params.set("after", String(opts.after));
    if (opts.before != null) params.set("before", String(opts.before));
    if (opts.limit != null) params.set("limit", String(opts.limit));
    const qs = params.toString();
    const path = `/agents/${id}/external/history${qs ? `?${qs}` : ""}`;
    return this.json<ExternalHistoryResponse>(path);
  }
}

async function readErrorMessage(resp: Response): Promise<string> {
  try {
    const body = (await resp.json()) as { error?: { message?: string } };
    const message = body?.error?.message;
    if (message) return message;
  } catch {
    try {
      const text = await resp.text();
      if (text) return text;
    } catch {
      // fall through to statusText
    }
  }
  return resp.statusText;
}
