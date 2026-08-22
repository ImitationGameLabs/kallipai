// The conversation transcript model + reducer, shared by every transport
// (direct offline, relayed online). The external event vocabulary is split by
// destination channel (see the Rust projector): authored content (assistant
// messages) crosses the E2EE envelope inside a `TagmaReply::Event` and is
// persisted in chat_history; runtime signals (busy/idle presence, turn
// terminals, errors) cross the plaintext signal channel and are ephemeral.
// Accordingly the transcript is driven by TWO reducer entry points:
//
// - `applyTagmaReply` over the wire `TagmaReply` (acks, op errors, replayed
//   user messages, and authored `assistant_content`).
// - `applySignal` over the plaintext `SignalEvent` (status transitions and the
//   transient system-message lines terminals/errors produce).
//
// Both are pure and unit-tested in transcript_test.ts.
//
// Lines are keyed by `historyId` (the tagma `chat_history.id`): a stable,
// monotonic id that doubles as the Svelte `{#each}` key and the cache key.
// Pending optimistic user lines (sent but not yet ack'd) carry a synthetic
// negative id minted by the store; they are never cached and are replaced by
// the real id when the `MessageAccepted` ack lands. Signal-produced system
// lines are also synthetic (signals carry no history id) and are never cached.
// Signal-produced text is localized at signal time under the active locale;
// signals are transient, so a mid-error locale switch keeps the old language.

import type {
  AuthoredEvent,
  Participant,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallip-lesche-client";
import {
  signal_failover_error,
  signal_failover_line,
  signal_max_rounds_error,
  signal_max_rounds_line,
  signal_token_budget_error,
  signal_token_budget_line,
  signal_turn_cancelled,
  signal_turn_interrupted,
} from "../paraglide/messages.js";

type ConversationRole = "user" | "assistant" | "system";

/** The UI-facing sender, derived from the wire `Participant`: the `kind`, a
 * single `id` (the opaque participant id), and the display `handle`. The UI
 * layer sees one flat `id`. The wire `kind` (`"human"`/`"agent"`) is mapped to
 * the UI label (`"user"`/`"agent"`) so the offline-direct chat's rendering +
 * tests keep their existing vocabulary. */
export interface ConversationSender {
  readonly kind: "user" | "agent";
  readonly id: string;
  readonly handle: string;
}

/** The fixed sender for the offline (direct) path's optimistic user bubble. The
 *  operator is anonymous on the direct path -- there is no enrolled identity --
 *  so this is a local placeholder that only the optimistic line reads. The wire
 *  always resolves a real sender from the history rows. */
export const LOCAL_OPERATOR_SENDER: ConversationSender = {
  kind: "user",
  id: "local-operator",
  handle: "Operator",
};

/** Derive the UI sender from the wire participant. */
export function toSender(participant: Participant): ConversationSender {
  return {
    kind: participant.kind === "human" ? "user" : "agent",
    id: participant.id,
    handle: participant.handle,
  };
}

export interface ConversationLine {
  /** Stable id: the tagma `chat_history.id` for confirmed lines, or a synthetic
   * negative id for pending optimistic user lines and signal-produced system
   * lines. Doubles as the `{#each}` key and (when positive) the cache key.
   * Unique within a transcript. */
  readonly historyId: number;
  readonly role: ConversationRole;
  readonly text: string;
  /** Who authored the line. Absent on signal-produced system lines (no sender)
   * and on old cached rows written before the sender was tracked. */
  readonly sender?: ConversationSender;
  /** RFC 3339 send time. For confirmed lines, the tagma row's `created_at`;
   * for an optimistic user line, the client-side render time until the ack
   * refines it. Absent on old cached rows and on signal-produced system lines. */
  readonly createdAt?: string;
  /** Per-line delivery status for an optimistic user line. Absent (≡ "sent")
   * for confirmed/replayed lines and for all non-user lines; `"sending"` from
   * the moment the line is rendered until its `MessageAccepted` ack lands. */
  readonly status?: "sending" | "sent";
}

type ConversationStatus = "idle" | "busy" | "error";

export interface ConversationTranscript {
  readonly lines: ConversationLine[];
  readonly status: ConversationStatus;
  /** Set when status === "error" (or a non-fatal notice); the chat view shows
   * it inline. */
  readonly error?: string;
}

export const EMPTY_TRANSCRIPT: ConversationTranscript = {
  lines: [],
  status: "idle",
};

/** Append one line with an explicit `historyId`, preserving status + error.
 * No-op for empty/whitespace text. */
function line(
  state: ConversationTranscript,
  historyId: number,
  role: ConversationRole,
  text: string,
  sender: ConversationSender | undefined,
  createdAt?: string,
): ConversationTranscript {
  const trimmed = text.trim();
  if (trimmed === "") return state;
  return {
    ...state,
    lines: [
      ...state.lines,
      { historyId, role, text: trimmed, sender, createdAt },
    ],
  };
}

/** Apply one tagma reply to the transcript. `sender` is the wire participant
 * who authored the reply's content (the user for `user_message`, the agent for
 * `event`); `lineId` is the store-assigned id for any content line this reply
 * produces (the tagma `history_id` when > 0, else a synthetic negative id the
 * store mints). Pure; returns a new state. */
export function applyTagmaReply(
  state: ConversationTranscript,
  reply: TagmaReply,
  sender: Participant | undefined,
  lineId: number,
): ConversationTranscript {
  const cs = sender ? toSender(sender) : undefined;
  switch (reply.kind) {
    case "message_accepted":
      // Informational ack (queue depth / warning); the store stamps the
      // optimistic user line with the ack's history_id separately.
      return state;
    case "interrupted":
      // Ack of an Interrupt op; the lifecycle Interrupted signal is what the
      // user sees (delivered via the signal channel, not this reply).
      return state;
    case "history_batch_end":
      return state;
    case "manage_result":
      // Management op response; intercepted in RelayChannel.enqueue and never
      // reaches the transcript reducer. No-op if it somehow arrives here.
      return state;
    case "error":
      return {
        ...line(state, lineId, "system", reply.message, undefined),
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
        cs,
        reply.created_at,
      );
    case "event":
      return applyAuthored(state, reply.event, cs, lineId, reply.created_at);
  }
}

/** Apply an authored event (a complete assistant message). Appends one line. */
function applyAuthored(
  state: ConversationTranscript,
  event: AuthoredEvent,
  sender: ConversationSender | undefined,
  lineId: number,
  createdAt?: string,
): ConversationTranscript {
  return line(state, lineId, "assistant", event.content, sender, createdAt);
}

/** The human-readable system line a signal produces, or `null` if it is
 * content-less (a pure status transition like `busy`/`idle`). Signal-produced
 * lines are transient (not cached, not replayed). */
function signalSystemLine(signal: SignalEvent): { text: string } | null {
  switch (signal.type) {
    case "error":
      return { text: signal.message };
    case "interrupted":
      return { text: signal_turn_interrupted() };
    case "cancelled":
      return { text: signal_turn_cancelled() };
    case "token_budget_exceeded":
      return {
        text: signal_token_budget_line({
          consumed: signal.consumed,
          budget: signal.budget,
        }),
      };
    case "max_rounds_exceeded":
      return { text: signal_max_rounds_line() };
    case "failover_chain_exhausted":
      return {
        text: signal_failover_line({
          reason: signal.reason,
          detail: signal.detail,
        }),
      };
    case "busy":
    case "idle":
      return null;
  }
}

/** Apply one runtime signal to the transcript. `lineId` is the store-assigned
 * synthetic id for any system line this signal produces. Pure; returns a new
 * state. Status transitions (busy/idle) and terminal/error system lines all
 * arrive here — they no longer ride the encrypted envelope. */
export function applySignal(
  state: ConversationTranscript,
  signal: SignalEvent,
  lineId: number,
): ConversationTranscript {
  const content = signalSystemLine(signal);
  const withLine = content
    ? line(state, lineId, "system", content.text, undefined)
    : state;
  switch (signal.type) {
    case "busy":
      // A new turn clears any stale error from the previous one.
      return { ...state, status: "busy", error: undefined };
    case "idle":
      // The agent yielded control. Content-less: just transition to idle.
      return { ...state, status: "idle", error: undefined };
    case "error":
      return { ...withLine, status: "error", error: signal.message };
    case "interrupted":
      return { ...withLine, status: "idle", error: undefined };
    case "cancelled":
      return { ...withLine, status: "idle", error: undefined };
    case "token_budget_exceeded":
      return {
        ...withLine,
        status: "error",
        error: signal_token_budget_error(),
      };
    case "max_rounds_exceeded":
      return {
        ...withLine,
        status: "error",
        error: signal_max_rounds_error(),
      };
    case "failover_chain_exhausted":
      return {
        ...withLine,
        status: "error",
        error: signal_failover_error(),
      };
  }
}

/** Append a pending user line (synthetic negative `localId`, status
 * `"sending"`) and mark the channel busy (a turn is starting). The store
 * replaces `localId` with the real `history_id` and flips status to `"sent"`
 * when the `MessageAccepted` ack lands. `sender` is the local user (online: the
 * agora session; offline: the tagma-configured local identity). */
export function withUserLine(
  state: ConversationTranscript,
  text: string,
  localId: number,
  sender: ConversationSender,
  now: Date = new Date(),
): ConversationTranscript {
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
        sender,
        // Client-side render time (millis precision); the ack refines this to
        // the server's whole-second `created_at` via `replaceLineId`. The
        // precision gap is invisible to the minute-granularity formatter.
        createdAt: now.toISOString(),
        status: "sending",
      },
    ],
    status: "busy",
  };
}

/** Replace the pending line carrying `localId` with a confirmed `historyId`
 * (the inbound row id from the `MessageAccepted` ack) and flip its status to
 * `"sent"`. `createdAt`, when given, refines the optimistic client-side stamp
 * to the server's authoritative send time. `sender`, when given, overwrites the
 * optimistic line's sender with the authoritative wire sender (so a handle that
 * changed mid-session does not freeze on the stale optimistic value). No-op if
 * the pending line is gone (already replaced, or cleared on reconnect), or if a
 * line with `historyId` already exists (an ack id colliding with an
 * already-rendered line would otherwise duplicate the Svelte/cache key). */
export function replaceLineId(
  state: ConversationTranscript,
  localId: number,
  historyId: number,
  createdAt?: string,
  sender?: ConversationSender,
): ConversationTranscript {
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
            ...(sender !== undefined ? { sender } : {}),
            ...(createdAt !== undefined ? { createdAt } : {}),
          }
        : l,
    ),
  };
}

/** Flip the optimistic user line carrying `localId` from `"sending"` to
 * `"sent"` without changing its id. Used when a `user_message` echo arrives
 * unstamped (`history_id === 0`) — e.g. a direct-path echo — so the line keeps
 * its synthetic id and only its status flips. No-op if the line is gone or
 * already resolved. */
export function markLineSent(
  state: ConversationTranscript,
  localId: number,
): ConversationTranscript {
  if (!state.lines.some((l) => l.historyId === localId)) return state;
  return {
    ...state,
    lines: state.lines.map((l) =>
      l.historyId === localId && l.status === "sending"
        ? { ...l, status: "sent" }
        : l,
    ),
  };
}

/** The cacheable content line for a reply, or `null` if it carries no durable
 * authored content (acks, batch markers, op errors) or has no real
 * `history_id` (synthetic / un-stored frames are not cached). Only authored
 * `assistant_content` and replayed `user_message` rows are cached — signals are
 * ephemeral and never cached. `sender` is the wire participant who authored the
 * row; persisted alongside the content so the cache-hydrate path renders the
 * author without a server round-trip. */
export function cacheLineOf(
  reply: TagmaReply,
  sender: Participant | undefined,
): {
  historyId: number;
  role: ConversationRole;
  text: string;
  sender?: ConversationSender;
  createdAt?: string;
} | null {
  const cs = sender ? toSender(sender) : undefined;
  if (reply.kind === "user_message") {
    return reply.history_id > 0
      ? {
          historyId: reply.history_id,
          role: "user",
          text: reply.text,
          sender: cs,
          createdAt: reply.created_at,
        }
      : null;
  }
  if (reply.kind === "event") {
    const id = reply.history_id ?? 0;
    if (id <= 0) return null;
    // reply.event is AuthoredEvent (assistant_content only).
    return {
      historyId: id,
      role: "assistant",
      text: reply.event.content,
      sender: cs,
      createdAt: reply.created_at,
    };
  }
  return null;
}
