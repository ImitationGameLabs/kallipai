import {
  applyEvent,
  EMPTY_TRANSCRIPT,
  isBoundary,
  withUserLine,
} from "@kallipai/kallip-common";
import type {
  ApprovalEntry,
  Session,
  TranscriptState,
} from "@kallipai/kallip-common";

function messageOf(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}

/**
 * Upsert an approval into the list by id, preserving the `order: 'desc'`
 * invariant the seed establishes. An event/update can arrive for an entry older
 * than the current head, so a blind prepend would misorder; insert before the
 * first entry whose `createdAt` is older.
 *
 * Timestamps are compared as epoch milliseconds, not lexically: the daemon's
 * RFC3339 serialization (Rust `time` rfc3339) omits the fractional part when
 * sub-second is zero, so a lexical compare would sort a whole-second timestamp
 * after a fractional one in the same second — an inversion. `Date.parse` on
 * RFC3339 fails safe to `NaN` (then `NaN < t` is false → append at tail).
 */
function upsertById(
  list: ApprovalEntry[],
  entry: ApprovalEntry,
): ApprovalEntry[] {
  const i = list.findIndex((a) => a.id === entry.id);
  if (i === -1) {
    const t = Date.parse(entry.createdAt);
    const j = list.findIndex((a) => Date.parse(a.createdAt) < t);
    return j === -1
      ? [...list, entry]
      : [...list.slice(0, j), entry, ...list.slice(j)];
  }
  const next = list.slice();
  next[i] = entry;
  return next;
}

/**
 * The app's session state, held as a Svelte 5 rune class singleton. Owns the
 * active {@link Session}, the transcript (driven by the common reducer), and the
 * pending-input queue that is flushed at turn boundaries — the web counterpart
 * of kallip-tui's App + send pipeline.
 */
class SessionStore {
  session: Session | null = $state(null);
  transcript: TranscriptState = $state(EMPTY_TRANSCRIPT);
  pending: string[] = $state([]);
  // Raw error object (TransportError/KallipError/other); the layout banner
  // classifies it for display. Unknown so any thrown value is preserved.
  error: unknown = $state(null);
  connecting = $state(false);
  approvals: ApprovalEntry[] = $state([]);
  // Distinguishes "still loading" from "loaded, empty". Reset on every attach.
  approvalsLoaded = $state(false);
  // List-fetch error only; kept separate from `error` (the connection banner)
  // so a list failure does not clear the connection status and vice-versa.
  approvalsError: string | null = $state(null);

  get connected(): boolean {
    return this.session !== null;
  }
  get busy(): boolean {
    return this.transcript.agentBusy;
  }

  /**
   * Record a connection error only if `session` is still the live session. A
   * stale attach/send/interrupt superseded by a newer attach() or torn down by
   * detach() has this.session reassigned (or nulled) synchronously before its
   * async error resolves, so the guard stops it from clobbering the live
   * connection. Callers must pass the session captured at call start, not
   * this.session re-read after the await. Logging is centralised in the layout.
   */
  private recordError(session: Session | null, e: unknown): void {
    if (this.session !== session) return;
    this.error = e;
  }

  /** Subscribe to the session's event stream and reduce it into the transcript. */
  async attach(session: Session): Promise<void> {
    this.detach();
    this.session = session;
    this.transcript = EMPTY_TRANSCRIPT;
    this.pending = [];
    this.error = null;
    this.approvals = [];
    this.approvalsLoaded = false;
    this.approvalsError = null;
    try {
      for await (const event of session.events) {
        this.transcript = applyEvent(this.transcript, event);
        // approvalUpdated is a no-op in applyEvent (reserved for this
        // view); react here. Fire-and-forget so a getApproval round-trip
        // never backpressures the single SSE consumer (which would stall
        // token deltas). Out-of-order resolutions are harmless: each
        // upserts the full entry by id, and status is monotonic.
        if (event.type === "approvalUpdated")
          void this.upsertApproval(event.id);
        if (isBoundary(event)) await this.flushPending();
      }
    } catch (e) {
      this.recordError(session, e);
    } finally {
      if (this.session === session) this.session = null;
    }
  }

  /** Send a prompt, or queue it if the agent is mid-turn. */
  async send(text: string): Promise<void> {
    const session = this.session;
    const trimmed = text.trim();
    if (!session || !trimmed) return;
    if (this.transcript.agentBusy) {
      this.pending = [...this.pending, trimmed];
      return;
    }
    this.transcript = withUserLine(this.transcript, trimmed);
    try {
      await session.send(trimmed);
    } catch (e) {
      this.recordError(session, e);
    }
  }

  async interrupt(): Promise<void> {
    const session = this.session;
    try {
      await session?.interrupt?.();
    } catch (e) {
      this.recordError(session, e);
    }
  }

  /** Fetch the approval list (all agents). Callable on direct transport only. */
  async refreshApprovals(): Promise<void> {
    const session = this.session;
    if (!session?.listApprovals) return;
    this.approvalsError = null;
    try {
      const res = await session.listApprovals({ limit: 20, order: "desc" });
      this.approvals = res.items;
      this.approvalsLoaded = true;
    } catch (e) {
      // Leave the stale list + approvalsLoaded as-is so a refresh failure
      // does not blank what is already on screen.
      this.approvalsError = messageOf(e);
    }
  }

  /** Approve a committed approval. Throws on failure; the caller renders it. */
  async approve(id: string): Promise<void> {
    await this.respond(id, "approve");
  }

  /** Deny a committed approval, with an optional reason. Throws on failure. */
  async deny(id: string, reason?: string): Promise<void> {
    await this.respond(id, "deny", reason);
  }

  /**
   * POST the decision, then refetch the single approval so the row flips status
   * without waiting for an SSE event. This is what makes sub-agent approvals
   * (whose events never arrive on the attached agent's stream) feel responsive.
   * The daemon emits the approvalUpdated SSE before returning the POST 200, so
   * this optimistic read returns the already-applied status; a concurrently
   * in-flight SSE upsert for the same id is last-writer-wins and status is
   * monotonic, so it self-heals within one tick.
   */
  private async respond(
    id: string,
    decision: "approve" | "deny",
    reason?: string,
  ): Promise<void> {
    const session = this.session;
    if (!session?.respondApproval) return;
    await session.respondApproval(id, decision, reason);
    await this.upsertApproval(id);
  }

  /** Fetch one approval and merge it into the list. Swallows transient errors. */
  private async upsertApproval(id: string): Promise<void> {
    const session = this.session;
    if (!session?.getApproval) return;
    try {
      const entry = await session.getApproval(id);
      this.approvals = upsertById(this.approvals, entry);
    } catch {
      // A transient getApproval failure on a live event must not flap the
      // list banner; the next refresh reconciles.
    }
  }

  detach(): void {
    this.session?.close().catch(() => {});
    this.session = null;
    this.transcript = EMPTY_TRANSCRIPT;
    this.pending = [];
    this.approvals = [];
    this.approvalsLoaded = false;
    this.approvalsError = null;
  }

  private async flushPending(): Promise<void> {
    const session = this.session;
    if (!session || this.pending.length === 0) return;
    const text = this.pending.join("\n");
    this.pending = [];
    this.transcript = withUserLine(this.transcript, text);
    try {
      await session.send(text);
    } catch (e) {
      this.recordError(session, e);
    }
  }
}

export const sessionStore = new SessionStore();
