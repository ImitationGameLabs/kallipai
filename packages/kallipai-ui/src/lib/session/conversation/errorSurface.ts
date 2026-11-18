// What a chat page's error line and composer note should say, given the
// transcript's own failure and the link's classified copy.
//
// Pure on purpose: no runes, no i18n. The page maps the returned notice to a
// message, so the rule about what stands where lives in one place instead of
// three inline derivations -- and one input says whether the failure's cause
// is already stated above this page: on the local conversation the shell
// banner speaks it, so the page's own line stays empty there.

import type { ComposerNotice } from "../composerState.ts";
import type { ConversationStatus } from "../../transcript.ts";

/** The line above the composer, the status the view colours the chatter
 *  with, and the note the composer still owes the reader. */
export interface ErrorSurface {
  readonly error?: string;
  readonly status: ConversationStatus;
  readonly notice: ComposerNotice | null;
}

export interface ErrorSurfaceInput {
  /** The transcript's own error line, when it has a non-empty one. */
  readonly transcriptError?: string;
  readonly transcriptStatus: ConversationStatus;
  /** The link failure's classified copy, or null when it has none to show. */
  readonly linkCopy: string | null;
  /** True when a surface above this page already states this link's cause
   *  (the shell banner does, for the local conversation). */
  readonly causeSpokenAbove: boolean;
  /** What the composer would say on the write axis alone. */
  readonly notice: ComposerNotice | null;
}

/** The transcript's own error wins: it is about the rows the reader sees.
 *  The link copy fills the line only when a surface above has not already
 *  stated that cause.
 *
 *  The link's copy on the line silences the composer's note, because both
 *  would answer the same question; the note's Retry action lives outside
 *  it and stays. The transcript's own error line leaves the note alone: it
 *  answers a different question, so the two can stand together.
 *
 *  "opening" and a link failure cannot share a screen, the one cell where
 *  the two would collide: `composerState` hands out "opening" only while the
 *  link status is "opening", and a failed drain writes "error" before any
 *  rebuild passes through "reconnecting".
 *
 *  The status is the transcript's own unless the line falls to the link's
 *  copy; then it becomes "error". Line and status are computed in one
 *  expression, so the two cannot drift apart. */
export function errorSurface(i: ErrorSurfaceInput): ErrorSurface {
  const lineCopy = i.causeSpokenAbove ? null : i.linkCopy;
  const error = i.transcriptError || lineCopy || undefined;
  const status: ConversationStatus =
    i.transcriptStatus === "error" || lineCopy !== null
      ? "error"
      : i.transcriptStatus;
  const notice = lineCopy === null ? i.notice : null;
  return { error, status, notice };
}
