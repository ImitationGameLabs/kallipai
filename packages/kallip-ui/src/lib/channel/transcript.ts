// Online-channel transcript model + reducer. This is the independent online
// path: NOT @kallipai/kallip-common's TranscriptState (which is shaped by the
// tagma's full event vocabulary + streaming). The agora path has no streaming
// (the tagma relay does not surface streaming deltas — it maps
// `AssistantContentDelta` to `None` at the app-facing event boundary), so each
// `assistant_content` is a complete message appended as its own line, and
// `idle` is a content-less status transition.
//
// `applyTagmaReply` is a pure reducer over the wire `TagmaReply` (from
// @kallipai/kallip-lesche-client). It is the only place that interprets a
// TagmaReply into view state, so it is unit-tested in transcript_test.ts.
//
// Lines are keyed by `historyId` (the tagma `chat_history.id`): a stable,
// monotonic id that doubles as the Svelte `{#each}` key and the cache key.
// Pending optimistic user lines (sent but not yet ack'd) carry a synthetic
// negative id minted by the store; they are never cached and are replaced by
// the real id when the `MessageAccepted` ack lands.

import type { TagmaEvent, TagmaReply } from "@kallipai/kallip-lesche-client";

type ChannelRole = "user" | "assistant" | "system";

export interface ChannelLine {
  /** Stable id: the tagma `chat_history.id` for confirmed lines, or a synthetic
   * negative id for pending optimistic user lines. Doubles as the `{#each}` key
   * and (when positive) the cache key. Unique within a transcript. */
  readonly historyId: number;
  readonly role: ChannelRole;
  readonly text: string;
  /** RFC 3339 send time. For confirmed lines, the tagma row's `created_at`;
   * for an optimistic user line, the client-side render time until the ack
   * refines it. Absent on old cached rows and on system/status lines. */
  readonly createdAt?: string;
  /** Per-line delivery status for an optimistic user line. Absent (≡ "sent")
   * for confirmed/replayed lines and for all non-user lines; `"sending"` from
   * the moment the line is rendered until its `MessageAccepted` ack lands. */
  readonly status?: "sending" | "sent";
}

type ChannelStatus = "idle" | "busy" | "error";

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

/** Append one line with an explicit `historyId`, preserving status + error.
 * No-op for empty/whitespace text. */
function line(
  state: ChannelTranscript,
  historyId: number,
  role: ChannelRole,
  text: string,
  createdAt?: string,
): ChannelTranscript {
  const trimmed = text.trim();
  if (trimmed === "") return state;
  return {
    ...state,
    lines: [...state.lines, { historyId, role, text: trimmed, createdAt }],
  };
}

/** The content line an event produces, or `null` if it is content-less (a pure
 * status transition like `busy`/`idle`). Extracted so the cache writer and the
 * reducer share one source of truth for what becomes a durable line. */
function contentLineForEvent(
  event: TagmaEvent,
): { role: ChannelRole; text: string } | null {
  switch (event.type) {
    case "assistant_content":
      return { role: "assistant", text: event.content };
    case "status":
      return { role: "system", text: event.message };
    case "error":
      return { role: "system", text: event.message };
    case "interrupted":
      return { role: "system", text: "Turn interrupted." };
    case "cancelled":
      return { role: "system", text: "Turn cancelled." };
    case "token_budget_exceeded":
      return {
        role: "system",
        text: `Token budget exceeded (consumed ${event.consumed} of ${event.budget}).`,
      };
    case "max_rounds_exceeded":
      return { role: "system", text: "Max tool rounds exceeded." };
    case "failover_chain_exhausted":
      return {
        role: "system",
        text: `Model failover exhausted (${event.reason}): ${event.detail}`,
      };
    case "busy":
    case "idle":
      return null;
  }
}

/** Apply one tagma reply to the transcript. `lineId` is the store-assigned id
 * for any content line this reply produces (the tagma `history_id` when > 0,
 * else a synthetic negative id the store mints). Pure; returns a new state. */
export function applyTagmaReply(
  state: ChannelTranscript,
  reply: TagmaReply,
  lineId: number,
): ChannelTranscript {
  switch (reply.kind) {
    case "message_accepted":
      // Informational ack (queue depth / warning); the store stamps the
      // optimistic user line with the ack's history_id separately.
      return state;
    case "interrupted":
      // Ack of an Interrupt op; the lifecycle Interrupted event below is what
      // the user sees.
      return state;
    case "history_batch_end":
      // A History-batch completion marker; the store uses it to flip back to
      // live draining. No transcript effect.
      return state;
    case "error":
      return {
        ...line(state, lineId, "system", reply.message),
        status: "error",
        error: reply.message,
      };
    case "user_message":
      // Replay-only echo of a user-authored message.
      return line(
        state,
        reply.history_id,
        "user",
        reply.text,
        reply.created_at,
      );
    case "event":
      return applyTagmaEvent(state, reply.event, lineId, reply.created_at);
  }
}

function applyTagmaEvent(
  state: ChannelTranscript,
  event: TagmaEvent,
  lineId: number,
  createdAt?: string,
): ChannelTranscript {
  const content = contentLineForEvent(event);
  const withLine = content
    ? line(state, lineId, content.role, content.text, createdAt)
    : state;
  switch (event.type) {
    case "busy":
      // A new turn clears any stale error from the previous one.
      return { ...state, status: "busy", error: undefined };
    case "assistant_content":
      // A complete (non-streamed) assistant message — also the variant
      // the `kallip lesche send` CLI's deliveries map to. Append as its own
      // line; the agent stays busy until the `idle` event.
      return withLine;
    case "idle":
      // The agent yielded control (called `break`). Content-less: just
      // transition to idle.
      return { ...state, status: "idle", error: undefined };
    case "status":
      return withLine;
    case "error":
      return {
        ...withLine,
        status: "error",
        error: event.message,
      };
    case "interrupted":
      return { ...withLine, status: "idle", error: undefined };
    case "cancelled":
      return { ...withLine, status: "idle", error: undefined };
    case "token_budget_exceeded":
      return {
        ...withLine,
        status: "error",
        error: "Token budget exceeded",
      };
    case "max_rounds_exceeded":
      return {
        ...withLine,
        status: "error",
        error: "Max tool rounds exceeded",
      };
    case "failover_chain_exhausted":
      return {
        ...withLine,
        status: "error",
        error: "Model failover exhausted",
      };
  }
}

/** Append a pending user line (synthetic negative `localId`, status
 * `"sending"`) and mark the channel busy (a turn is starting). The store
 * replaces `localId` with the real `history_id` and flips status to `"sent"`
 * when the `MessageAccepted` ack lands. */
export function withUserLine(
  state: ChannelTranscript,
  text: string,
  localId: number,
): ChannelTranscript {
  const trimmed = text.trim();
  if (trimmed === "") return state;
  return {
    ...state,
    lines: [
      ...state.lines,
      {
        historyId: localId,
        role: "user",
        text: trimmed,
        // Client-side render time (millis precision); the ack refines this to
        // the server's whole-second `created_at` via `replaceLineId`. The
        // precision gap is invisible to the minute-granularity formatter.
        createdAt: new Date().toISOString(),
        status: "sending",
      },
    ],
    status: "busy",
  };
}

/** Replace the pending line carrying `localId` with a confirmed `historyId`
 * (the inbound row id from the `MessageAccepted` ack) and flip its status to
 * `"sent"`. `createdAt`, when given, refines the optimistic client-side stamp
 * to the server's authoritative send time. No-op if the pending line is gone
 * (already replaced, or cleared on reconnect), or if a line with `historyId`
 * already exists (an ack id colliding with an already-rendered line would
 * otherwise duplicate the Svelte/cache key). */
export function replaceLineId(
  state: ChannelTranscript,
  localId: number,
  historyId: number,
  createdAt?: string,
): ChannelTranscript {
  if (!state.lines.some((l) => l.historyId === localId)) return state;
  if (state.lines.some((l) => l.historyId === historyId)) return state;
  return {
    ...state,
    lines: state.lines.map((l) =>
      l.historyId === localId
        ? {
            ...l,
            historyId,
            status: "sent",
            ...(createdAt !== undefined ? { createdAt } : {}),
          }
        : l,
    ),
  };
}

/** The cacheable content line for a reply, or `null` if it carries no durable
 * content (acks, batch markers, status-only events) or has no real `history_id`
 * (synthetic / un-stored frames are not cached). The single source of truth for
 * what the cache persists, shared with the reducer via `contentLineForEvent`. */
export function cacheLineOf(reply: TagmaReply): {
  historyId: number;
  role: ChannelRole;
  text: string;
  createdAt?: string;
} | null {
  if (reply.kind === "user_message") {
    return reply.history_id > 0
      ? {
          historyId: reply.history_id,
          role: "user",
          text: reply.text,
          createdAt: reply.created_at,
        }
      : null;
  }
  if (reply.kind === "event") {
    const id = reply.history_id ?? 0;
    if (id <= 0) return null;
    const content = contentLineForEvent(reply.event);
    return content
      ? {
          historyId: id,
          role: content.role,
          text: content.text,
          createdAt: reply.created_at,
        }
      : null;
  }
  return null;
}
