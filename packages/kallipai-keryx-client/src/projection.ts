// Projection read client: the keryx's cached, prompt-free
// snapshot of a tagma plus the per-tagma dirty SSE. Reads are served from the
// keryx's store (offline tags keep serving, flagged `stale`), so the
// browser no longer needs to poll the tagma's manage plane for roster/status.
// All endpoints are owner-gated with the shared session cookie.

import {
  type DataPlaneFailure,
  eventFrames,
  runClientStream,
} from "@kallipai/kallipai-common";
import { Result, Stream } from "effect";
import type {
  ProjectionAgentsResponse,
  ProjectionBudgetResponse,
  ProjectionDirty,
  ProjectionStatusResponse,
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

/** `GET /tagmata/{id}/state` as a stream of dirty nudges. The handler sends
 * `Event::default().json_data(dirty)` -- no `event:` name, no `id:` -- so this
 * endpoint has neither the open marker nor cursor fields. The filter below
 * stays as the guard a payload frame needs; dropping the marker is what it
 * would do on an endpoint that sends one. A refusal, a dead body or a frame
 * that is not JSON ends that attempt with the seam's own failure, which the
 * consumer's backoff reconnects past. */
export function projectionState(
  base: string,
  id: string,
): Stream.Stream<ProjectionDirty, DataPlaneFailure> {
  return eventFrames<ProjectionDirty>(
    `${base}/tagmata/${encodeURIComponent(id)}/state`,
  ).pipe(
    Stream.filterMap((frame) =>
      frame.kind === "event" ? Result.succeed(frame.event) : Result.fail(frame),
    ),
  );
}

/** Read-side client for a tagma's projection state: four cached GETs
 * (agents/budget/work-schedule/status) plus the dirty-frame SSE. Constructed
 * with the keryx base URL (the service the conversation verbs dial); the
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

  /** `GET /tagmata/{id}/status` -- the presence cache's latest relayed
   * snapshot; an offline tagma serves its stored projection (`stale`). */
  async status(id: string): Promise<ProjectionStatusResponse> {
    return (await this.get(id, "status")) as ProjectionStatusResponse;
  }

  /** The generator face of {@linkcode projectionState}: same nudges, same
   * failure channel surfaced as a thrown `DataPlaneApiError`/`TransportError`,
   * and it ends cleanly when `signal` aborts. */
  async *state(
    id: string,
    signal?: AbortSignal,
  ): AsyncGenerator<ProjectionDirty> {
    yield* runClientStream(projectionState(this.baseUrl, id), signal);
  }

  // Off the seam on purpose: a read returns a bare JSON body the class asserts
  // to its own shape instead of parsing it against a schema, so a refusal is
  // read as the status line and thrown as a bare `Error`, with no typed
  // failure to build. No read carries a signal either, so there is no
  // window to speak of.
  private async get(id: string, tail: string): Promise<unknown> {
    const resp = await fetch(
      `${this.baseUrl}/tagmata/${encodeURIComponent(id)}/${tail}`,
      { method: "GET", credentials: "include" },
    );
    if (!resp.ok) throw new Error(`projection ${tail}: ${resp.status}`);
    return resp.json();
  }
}
