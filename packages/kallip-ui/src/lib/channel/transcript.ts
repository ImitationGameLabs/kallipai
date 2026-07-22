// Online-channel transcript model + reducer. This is the independent online
// path: NOT @kallipai/kallip-common's TranscriptState (which is shaped by the
// daemon's full event vocabulary + streaming). The agora path has no streaming
// (the herald drops deltas), so each `assistant_content` / `finished` is a
// complete message, appended as its own line.
//
// `applyTagmaReply` is a pure reducer over the wire `TagmaReply` (from
// @kallipai/kallip-agora-client). It is the only place that interprets a
// TagmaReply into view state, so it is unit-tested in transcript_test.ts.

import type { TagmaEvent, TagmaReply } from "@kallipai/kallip-agora-client";

export type ChannelRole = "user" | "assistant" | "system";

export interface ChannelLine {
  /** Monotonic append index within this transcript. Append-only lines never
   * reorder, so the index is a stable identity for {#each} keys (two identical
   * "Turn interrupted." system lines must not share a key). */
  readonly seq: number;
  readonly role: ChannelRole;
  readonly text: string;
}

export type ChannelStatus = "idle" | "busy" | "error";

export interface ChannelTranscript {
  readonly lines: ChannelLine[];
  readonly status: ChannelStatus;
  /** Set when status === "error" (or a non-fatal notice); the chat view shows
   * it inline. */
  readonly error?: string;
}

export const EMPTY_TRANSCRIPT: ChannelTranscript = {
  lines: [],
  status: "idle",
};

/** Append one line, preserving status + error. No-op for empty/whitespace text. */
function line(
  state: ChannelTranscript,
  role: ChannelRole,
  text: string,
): ChannelTranscript {
  const trimmed = text.trim();
  if (trimmed === "") return state;
  return {
    ...state,
    lines: [...state.lines, { seq: state.lines.length, role, text: trimmed }],
  };
}

/** Apply one herald reply to the transcript. Pure; returns a new state. */
export function applyTagmaReply(
  state: ChannelTranscript,
  reply: TagmaReply,
): ChannelTranscript {
  switch (reply.kind) {
    case "message_accepted":
      // Informational ack (queue depth / warning). Not a transcript event.
      return state;
    case "interrupted":
      // Ack of an Interrupt op; the lifecycle Interrupted event below is what
      // the user sees.
      return state;
    case "error":
      return {
        ...line(state, "system", reply.message),
        status: "error",
        error: reply.message,
      };
    case "event":
      return applyTagmaEvent(state, reply.event);
  }
}

function applyTagmaEvent(
  state: ChannelTranscript,
  event: TagmaEvent,
): ChannelTranscript {
  switch (event.type) {
    case "busy":
      // A new turn clears any stale error from the previous one.
      return { ...state, status: "busy", error: undefined };
    case "assistant_content":
      // A complete (non-streamed) assistant message. Append as its own line;
      // status is unchanged (busy until `finished`).
      return line(state, "assistant", event.content);
    case "finished": {
      // Turn complete. The content is the final assistant message; if the
      // trailing assistant line already carries identical text (an immediately
      // preceding assistant_content), just go idle instead of duplicating.
      const last = state.lines[state.lines.length - 1];
      if (
        last &&
        last.role === "assistant" &&
        last.text === event.content.trim()
      ) {
        return { ...state, status: "idle", error: undefined };
      }
      return {
        ...line(state, "assistant", event.content),
        status: "idle",
        error: undefined,
      };
    }
    case "status":
      return line(state, "system", event.message);
    case "error":
      return {
        ...line(state, "system", event.message),
        status: "error",
        error: event.message,
      };
    case "interrupted":
      return {
        ...line(state, "system", "Turn interrupted."),
        status: "idle",
        error: undefined,
      };
    case "cancelled":
      return {
        ...line(state, "system", "Turn cancelled."),
        status: "idle",
        error: undefined,
      };
    case "token_budget_exceeded":
      return {
        ...line(
          state,
          "system",
          `Token budget exceeded (consumed ${event.consumed} of ${event.budget}).`,
        ),
        status: "error",
        error: "Token budget exceeded",
      };
    case "max_rounds_exceeded":
      return {
        ...line(state, "system", "Max tool rounds exceeded."),
        status: "error",
        error: "Max tool rounds exceeded",
      };
    case "failover_chain_exhausted":
      return {
        ...line(
          state,
          "system",
          `Model failover exhausted (${event.reason}): ${event.detail}`,
        ),
        status: "error",
        error: "Model failover exhausted",
      };
  }
}

/** Append a user line on send and mark the channel busy (a turn is starting). */
export function withUserLine(
  state: ChannelTranscript,
  text: string,
): ChannelTranscript {
  const trimmed = text.trim();
  if (trimmed === "") return state;
  return { ...line(state, "user", trimmed), status: "busy" };
}
