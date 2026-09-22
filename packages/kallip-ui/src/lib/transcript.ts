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
  FileAttachment,
  HistoryEntry,
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
  /** Per-line delivery status for an optimistic user line. Absent (equivalent
   * to "sent") for confirmed/replayed lines and for all non-user lines;
   * "sending" from the moment the line is rendered until its ack lands;
   * "failed" when the send never landed -- the line stays visible (with its
   * per-line error copy) as a retry candidate instead of vanishing. */
  readonly status?: "sending" | "sent" | "failed";
  readonly error?: string;
  /** The file attached to this message, when the sender shared one (an
   *  optimistic user line that carries it, or a replayed user row). Rendered
   *  as a file card; absent on plain-text lines and system/error lines. */
  readonly attachment?: FileAttachment;
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
 * No-op for empty/whitespace text unless an attachment rides along. */
function line(
  state: ConversationTranscript,
  historyId: number,
  role: ConversationRole,
  text: string,
  sender: ConversationSender | undefined,
  createdAt?: string,
  attachment?: FileAttachment,
): ConversationTranscript {
  const trimmed = text.trim();
  if (trimmed === "" && attachment === undefined) return state;
  return {
    ...state,
    lines: [
      ...state.lines,
      {
        historyId,
        role,
        text: trimmed,
        sender,
        createdAt,
        ...(attachment !== undefined ? { attachment } : {}),
      },
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
        reply.attachment,
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

/** Map one pulled history row to its transcript line, or null when the row
 * carries no renderable content (acks/markers/batch-end) or no durable id
 * (unstamped; the cache and the {#each} key both key on historyId). Mirrors
 * applyTagmaReply's mapping: event ⇒ assistant, user_message ⇒ user —
 * replayed rows must render identically to live ones. */
export function historyEntryLine(entry: HistoryEntry): ConversationLine | null {
  const { reply, sender } = entry;
  const cs = sender ? toSender(sender) : undefined;
  // TagmaReply is a discriminated union: history_id/created_at exist only on
  // the content arms, so narrow by kind before reading them.
  if (reply.kind === "event") {
    const historyId = reply.history_id ?? 0;
    if (historyId <= 0) return null;
    const text = reply.event.content.trim();
    if (text === "") return null;
    return {
      historyId,
      role: "assistant",
      text,
      sender: cs,
      createdAt: reply.created_at,
    };
  }
  if (reply.kind === "user_message") {
    const historyId = reply.history_id ?? 0;
    if (historyId <= 0) return null;
    const text = reply.text.trim();
    if (text === "" && reply.attachment === undefined) return null;
    return {
      historyId,
      role: "user",
      text,
      sender: cs,
      createdAt: reply.created_at,
      ...(reply.attachment !== undefined
        ? { attachment: reply.attachment }
        : {}),
    };
  }
  return null;
}

/** Merge one pulled history batch (oldest-first) into the transcript. One
 * single lines rebuild per batch — the batch, not each row, is the
 * granularity the live drain never offers. Rows whose id already sits in
 * the window are dropped (idempotent against re-pulls and cache/server
 * overlap); everything else is inserted in id order, before the durable
 * ids but always behind the optimistic tail (synthetic negative ids must
 * stay last so the sending pulse stays pinned to the bottom). */
export function mergeHistoryLines(
  state: ConversationTranscript,
  rows: ConversationLine[],
): { transcript: ConversationTranscript; added: number } {
  if (rows.length === 0) return { transcript: state, added: 0 };
  const durable = state.lines.filter((l) => l.historyId > 0);
  const tail = state.lines.filter((l) => l.historyId <= 0);
  const have = new Set(durable.map((l) => l.historyId));
  const fresh = rows.filter((l) => l.historyId > 0 && !have.has(l.historyId));
  if (fresh.length === 0) return { transcript: state, added: 0 };
  const lines = [...durable, ...fresh].sort(
    (a, b) => a.historyId - b.historyId,
  );
  return {
    transcript: { ...state, lines: [...lines, ...tail] },
    added: fresh.length,
  };
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
 * archeion session; offline: the tagma-configured local identity). */
export function withUserLine(
  state: ConversationTranscript,
  text: string,
  localId: number,
  sender: ConversationSender,
  attachment?: FileAttachment,
  now: Date = new Date(),
): ConversationTranscript {
  const trimmed = text.trim();
  if (trimmed === "" && attachment === undefined) return state;
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
        ...(attachment !== undefined ? { attachment } : {}),
      },
    ],
    status: "busy",
    // A fresh user line starts a new turn: clear any stale red error
    // a previous local send failure left (mirrors the busy-signal clear).
    error: undefined,
  };
}

/** Apply a local send failure: the line STAYS (status "failed", the copy
 * riding it) as the retry candidate -- it is never dropped and no history
 * line is appended. With a message, the transcript-wide red error joins
 * it; without one (a rehydrated row with no stored copy), only the line
 * marks failed. A genuine server kind:"error" reply (the wire path) still
 * enters history via `applyTagmaReply`; that double render is exactly why
 * this local failure, which never was a server reply, gets its own entry
 * point. */
export function sendFailed(
  state: ConversationTranscript,
  localId: number,
  message?: string,
): ConversationTranscript {
  return {
    ...state,
    // Keep the line, mark it failed: the user's words stay visible as a
    // retry candidate (the pending store holds the durable copy).
    lines: state.lines.map((l) =>
      l.historyId === localId
        ? { ...l, status: "failed" as const, error: message }
        : l,
    ),
    // The transcript-wide red banner is copy-driven: no stored copy (the
    // common rehydration case) marks the line failed without forcing the
    // whole transcript into the error state.
    ...(message ? { status: "error" as const, error: message } : {}),
  };
}

/** Re-arm a failed user line for another send attempt: back to "sending",
 * per-line error cleared (the transcript-wide red error goes with the next
 * turn). No-op when the line is gone or already re-armed. */
export function retryLine(
  state: ConversationTranscript,
  localId: number,
): ConversationTranscript {
  return {
    ...state,
    lines: state.lines.map((l) =>
      l.historyId === localId && l.status === "failed"
        ? { ...l, status: "sending" as const, error: undefined }
        : l,
    ),
  };
}

/** The next durable pending stamp: a negative millisecond mark (newer sends
 * compare smaller). The transcript renders in insertion order -- each send
 * appends to the tail -- so the stamp's job is uniqueness, and the pending
 * flush re-orders rows explicitly (oldest-first) before walking them.
 * Same-millisecond sends and clock steps just decrement: uniqueness is
 * what matters, not wall-clock fidelity. Pure so the allocation is
 * testable without IndexedDB. */
export function nextPendingSeq(nowMs: number, lastSeq: number): number {
  const candidate = -nowMs;
  return candidate < lastSeq ? candidate : lastSeq - 1;
}

/** True when `reply` is the op error answering the in-flight send: the
 * request's `req_id` (stamped by the channel on accept, echoed back by
 * `op_err_reply`) matches. Unmatched errors -- no in-flight send, no
 * stamped req_id (the direct path), or a different request -- return false
 * and fall through to the wire reducer's durable history record. */
export function isInFlightError(
  reply: TagmaReply,
  inFlight: { reqId?: number } | null,
): reply is Extract<TagmaReply, { kind: "error" }> {
  return (
    reply.kind === "error" &&
    reply.req_id > 0 &&
    inFlight?.reqId === reply.req_id
  );
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
  attachment?: FileAttachment;
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
          ...(reply.attachment !== undefined
            ? { attachment: reply.attachment }
            : {}),
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
