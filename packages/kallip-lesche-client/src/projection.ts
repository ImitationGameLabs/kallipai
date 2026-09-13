// Projection read client: the lesche's cached, prompt-free
// snapshot of a tagma plus the per-tagma dirty SSE. Reads are served from the
// lesche's store (offline tags keep serving, flagged `stale`), so the
// browser no longer needs to poll the tagma's manage plane for roster/status.
// All endpoints are owner-gated with the shared session cookie.

import { parseSseStream } from "@kallipai/kallip-common";
import { sseFetch } from "./http.ts";
import type {
  ProjectionAgentsResponse,
  ProjectionBudgetResponse,
  ProjectionDirty,
  ProjectionWorkScheduleResponse,
} from "./types.ts";

/** Linear backoff: 2s per consecutive failure, capped at 30s. Linear rather
 * than exponential because the dominant failure is a deterministic server
 * rejection (a 404 for an unenrolled tagma), where wide exponential gaps
 * only blunt recovery from a transient blip. `reset()` on any success. */
export class LinearBackoff {
  private failures = 0;
  constructor(
    private readonly stepMs = 2_000,
    private readonly capMs = 30_000,
  ) {}

  /** Milliseconds to wait before the next attempt (0 on first use). */
  next(): number {
    const delay = Math.min(this.failures * this.stepMs, this.capMs);
    this.failures += 1;
    return delay;
  }

  reset(): void {
    this.failures = 0;
  }
}

/** Read-side client for a tagma's projection state: three cached GETs
 * (agents/budget/work-schedule) plus the dirty-frame SSE. Constructed
 * with the lesche base URL (same origin as `LescheClient`); the session
 * cookie is the auth, so every fetch is credentialed. GET-only, hence no
 * CSRF marker. */
export class ProjectionClient {
  constructor(private readonly baseUrl: string) {}

  /** `GET /tagmata/{id}/agents` -- cached roster + status. */
  async agents(id: string): Promise<ProjectionAgentsResponse> {
    return (await this.get(id, "agents")) as ProjectionAgentsResponse;
  }

  /** `GET /tagmata/{id}/budget` -- cached budget snapshot. */
  async budget(id: string): Promise<ProjectionBudgetResponse> {
    return (await this.get(id, "budget")) as ProjectionBudgetResponse;
  }

  /** `GET /tagmata/{id}/work-schedule` -- cached schedule. */
  async workSchedule(id: string): Promise<ProjectionWorkScheduleResponse> {
    return (await this.get(
      id,
      "work-schedule",
    )) as ProjectionWorkScheduleResponse;
  }

  /** `GET /tagmata/{id}/state` -- the dirty-frame SSE: the tagma's
   * projection-state change channel. Each payload is a `ProjectionDirty`
   * nudge ({tagma_id, seq}, no content). A long-lived fetch parsed with
   * the shared `parseSseStream`; the caller owns reconnect/backoff; the
   * generator ends when the stream closes or `signal` aborts. */
  async *state(
    id: string,
    signal?: AbortSignal,
  ): AsyncGenerator<ProjectionDirty> {
    const resp = await sseFetch(
      `${this.baseUrl}/tagmata/${encodeURIComponent(id)}/state`,
      signal,
    );
    if (!resp.ok) throw new Error(`projection state: ${resp.status}`);
    for await (const ev of parseSseStream(resp, signal)) {
      // A torn or non-JSON frame is skipped: dirty frames are pure
      // nudges, so a lost frame only costs one debounced re-pull.
      try {
        yield JSON.parse(ev.data) as ProjectionDirty;
      } catch {
        continue;
      }
    }
  }

  private async get(id: string, tail: string): Promise<unknown> {
    const resp = await fetch(
      `${this.baseUrl}/tagmata/${encodeURIComponent(id)}/${tail}`,
      { method: "GET", credentials: "include" },
    );
    if (!resp.ok) throw new Error(`projection ${tail}: ${resp.status}`);
    return resp.json();
  }
}
