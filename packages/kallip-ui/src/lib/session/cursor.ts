// Client-side cursor tracking for the `GET /v1/lesche/me/events` stream.
// The lesche stamps every payload frame with an SSE `id` of `<epoch>:<seq>` and
// opens each connection with a marker carrying `{ epoch, next_seq }` (wire
// contract). This module turns those numbers into resync decisions — pure and
// data-in/data-out, so the policy is unit-testable without mocks.
//
// Resync policy:
// - `gap`: frames were lost. The dropped range is unknown client-side, so the
//   recovery is a targeted refetch of fetch-time resync grounds (rooms, roster,
//   open conversation tail) — not replay.
// - `full`: the epoch changed (lesche restarted / stream recreated). Everything
//   the stream feeds may be stale; refetch all of the above (status heals
//   itself on its 2s snapshot, so it is no resync ground).
// - No action for `tagma_status` (a 2s self-healing snapshot) or
//   `file_delivered` (lands in the user's files space; the files listing
//   recovers it on next open) — kept here so the policy stays explainable.

import type { MeEventFrame } from "@kallipai/kallip-lesche-client";

export type ResyncPlan = { kind: "gap" } | { kind: "full" };

/**
 * Tracks `(epoch, expectedSeq)` for one app-stream connection. Feed every
 * frame (marker and payload alike) to {@linkcode observe}; it returns a
 * resync plan when frames were lost or the stream regenerated, `null` when
 * the frame is in-order (or ignorable).
 *
 * Degradation: a payload frame without an `id` flips the tracker into
 * *unversioned* mode for the rest of the connection — gap detection turns
 * off and every further frame is accepted with no plan, which is exactly
 * the pre-cursor behavior. A later marker re-arms tracking (a server that
 * sends a marker is versioned).
 */
export class MeCursor {
  private epoch = 0;
  private expectedSeq = 0;
  private unversioned = false;
  private adopted = false;

  /**
   * Absorb a marker frame. Returns the resync plan the marker implies.
   *
   * - Epoch change (or first marker): the whole stream regenerated — `full`.
   * - Same epoch with `nextSeq > expectedSeq`: frames allocated during a
   *   disconnect window were lost — `gap`. This is the marker invariant
   *   doing its job: `next_seq` is the first seq the server allocates after
   *   the capture, so anything below it that we have not seen is gone.
   * - Otherwise: clean reconnect, no plan.
   *
   * Within one epoch, `expectedSeq` never rewinds (`max` clamp): the
   * client has already observed `expectedSeq - 1`, so the server counter
   * is necessarily `>= expectedSeq`; honouring a smaller number could
   * only come from an anomalous ordering and would re-arm false progress
   * on stale frames. An epoch change is the one reset: a regenerated
   * stream reallocates seqs from zero, so the marker's counter replaces
   * the old floor outright.
   */
  observe(frame: MeEventFrame): ResyncPlan | null {
    if (frame.kind === "stream") {
      const regenerated = this.adopted && frame.epoch !== this.epoch;
      const plan = regenerated
        ? ({ kind: "full" } as const)
        : this.adopted && frame.nextSeq > this.expectedSeq
          ? ({ kind: "gap" } as const)
          : null;
      this.epoch = frame.epoch;
      // A regenerated stream reallocates seqs from zero, so the marker's
      // counter becomes the floor wholesale; the `max` clamp only guards
      // the counter within one epoch (where it never rewinds).
      this.expectedSeq = regenerated
        ? frame.nextSeq
        : Math.max(this.expectedSeq, frame.nextSeq);
      this.adopted = true;
      this.unversioned = false;
      return plan;
    }
    const { epoch, seq } = frame;
    if (epoch === null || seq === null) {
      // No cursor on the wire: degrade, sticky for this connection.
      this.unversioned = true;
      return null;
    }
    if (this.unversioned) return null;
    if (epoch !== this.epoch) {
      // Defensive: payload frames of a stream this tracker never adopted.
      // A marker always precedes the flush on every connection, so this is
      // an anomalous ordering — treat like a regenerated stream.
      this.epoch = epoch;
      this.expectedSeq = seq + 1;
      return { kind: "full" };
    }
    if (seq < this.expectedSeq) return null; // duplicate or pre-adoption
    const plan = seq > this.expectedSeq ? ({ kind: "gap" } as const) : null;
    this.expectedSeq = seq + 1;
    return plan;
  }
}

/** Coalesces resync plans on a trailing timer so a burst of dropped frames
 * costs one refetch round, not one per frame. `full` covers `gap` (a
 * regenerated stream subsumes any narrower loss). The realtime store owns
 * one batcher; `flush()` fires early (store stop), `stop()` discards the
 * pending plan without firing. */
export class ResyncBatcher {
  private pending: ResyncPlan | null = null;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly fire: (plan: ResyncPlan) => void,
    private readonly delayMs = 250,
  ) {}

  push(plan: ResyncPlan): void {
    this.pending =
      this.pending?.kind === "full" || plan.kind === "full"
        ? { kind: "full" }
        : { kind: "gap" };
    if (this.timer === undefined) {
      this.timer = setTimeout(() => this.flush(), this.delayMs);
    }
  }

  /** Fire any pending plan now (also the timer's own path). */
  flush(): void {
    if (this.timer !== undefined) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    const plan = this.pending;
    this.pending = null;
    if (plan) this.fire(plan);
  }

  /** Drop the pending plan without firing (store stopped). */
  stop(): void {
    if (this.timer !== undefined) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    this.pending = null;
  }
}
