// Tests for the conversation transcript reducer. Authored content (assistant
// messages) arrives via applyTagmaReply (now paired with its wire sender);
// runtime signals (busy/idle, terminals, errors) arrive via applySignal. Each
// variant maps to the expected lines + status; assistant content is append-only
// (no streaming merge); busy/idle are content-less status transitions.

import { assert, assertEquals } from "@std/assert";
import {
  applySignal,
  applyTagmaReply,
  cacheLineOf,
  type ConversationLine,
  type ConversationTranscript,
  EMPTY_TRANSCRIPT,
  historyEntryLine,
  isInFlightError,
  markLineSent,
  mergeHistoryLines,
  nextPendingSeq,
  replaceLineId,
  retryLine,
  sendFailed,
  withUserLine,
} from "./transcript.ts";
import type {
  Participant,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallip-lesche-client";

const agentP: Participant = { id: "t1", kind: "agent", handle: "Tagma" };
const userP: Participant = { id: "u1", kind: "human", handle: "Alice" };
const agentS = { kind: "agent" as const, id: "t1", handle: "Tagma" };
const userS = { kind: "user" as const, id: "u1", handle: "Alice" };

/** Pick the wire sender matching a reply's content (agent for events, user for
 * user_message; undefined for acks/errors/markers). */
function senderFor(r: TagmaReply): Participant | undefined {
  if (r.kind === "event") return agentP;
  if (r.kind === "user_message") return userP;
  return undefined;
}

function reply(r: TagmaReply, lineId = 1): ConversationTranscript {
  return applyTagmaReply(EMPTY_TRANSCRIPT, r, senderFor(r), lineId);
}

function signal(s: SignalEvent, lineId = 1): ConversationTranscript {
  return applySignal(EMPTY_TRANSCRIPT, s, lineId);
}

Deno.test(
  "message_accepted / interrupted / history_batch_end are no-ops",
  () => {
    assertEquals(
      applyTagmaReply(
        EMPTY_TRANSCRIPT,
        { kind: "message_accepted", req_id: 1, queue_depth: 0 },
        undefined,
        1,
      ).lines.length,
      0,
    );
    assertEquals(
      applyTagmaReply(
        EMPTY_TRANSCRIPT,
        { kind: "interrupted", req_id: 1 },
        undefined,
        1,
      ).lines.length,
      0,
    );
    assertEquals(
      applyTagmaReply(
        EMPTY_TRANSCRIPT,
        { kind: "history_batch_end", req_id: 1, count: 0, more: false },
        undefined,
        1,
      ).lines.length,
      0,
    );
  },
);

Deno.test("TagmaReply error sets status error + a system line", () => {
  const t = reply({
    kind: "error",
    req_id: 2,
    status: 502,
    message: "tagma blew up",
  });
  assertEquals(t.status, "error");
  assertEquals(t.error, "tagma blew up");
  assertEquals(t.lines, [
    {
      historyId: 1,
      role: "system",
      text: "tagma blew up",
      sender: undefined,
      createdAt: undefined,
    },
  ]);
});

// A local send failure keeps the optimistic line (status "failed", the
// per-line error copy riding it) and sets the transcript-wide red error.
// The line is the retry candidate; the pending store holds its copy.
Deno.test("sendFailed keeps the line as a failed retry candidate", () => {
  const sending = withUserLine(EMPTY_TRANSCRIPT, "hello", -1, userS);
  const t = sendFailed(sending, -1, "post failed");
  assertEquals(t.lines, [
    {
      historyId: -1,
      role: "user",
      text: "hello",
      sender: userS,
      createdAt: t.lines[0]!.createdAt,
      status: "failed",
      error: "post failed",
      errorCode: undefined,
    },
  ]);
  assertEquals(t.status, "error");
  assertEquals(t.error, "post failed");
});

Deno.test(
  "sendFailed: coded rejection lands on the line and skips the banner",
  () => {
    const sending = withUserLine(EMPTY_TRANSCRIPT, "hello", -1, userS);
    const t = sendFailed(
      sending,
      -1,
      "operator verbatim",
      "profile_set_unusable",
    );
    assertEquals(t.lines[0]!.errorCode, "profile_set_unusable");
    assertEquals(t.status, sending.status); // banner state unchanged
    assertEquals(t.error, undefined);
  },
);

// No-copy variant (the rehydration path): the line marks failed so the
// bubble shows the error outline, but the transcript-wide red banner is
// not forced on -- there is nothing actionable to say at that moment.
Deno.test("sendFailed without copy skips the transcript-wide banner", () => {
  const sending = withUserLine(EMPTY_TRANSCRIPT, "hello", -1, userS);
  const t = sendFailed(sending, -1);
  assertEquals(t.lines[0]!.status, "failed");
  assertEquals(t.lines[0]!.error, undefined);
  assertEquals(t.status, "busy");
  assertEquals(t.error, undefined);
});

// retryLine re-arms the failed line for another attempt; a second call is
// a no-op (the line is no longer failed), the retry-send idempotency guard.
Deno.test("retryLine re-arms a failed line and is idempotent", () => {
  const failed = sendFailed(
    withUserLine(EMPTY_TRANSCRIPT, "hello", -1, userS),
    -1,
    "post failed",
  );
  const armed = retryLine(failed, -1);
  assertEquals(armed.lines[0]!.status, "sending");
  assertEquals(armed.lines[0]!.error, undefined);
  assertEquals(retryLine(armed, -1), armed);
});

// The pending stamp allocator: negative, strictly decreasing across calls,
// even for same-millisecond sends.
Deno.test("nextPendingSeq allocates strictly decreasing stamps", () => {
  const first = nextPendingSeq(1_000, 0);
  assertEquals(first, -1_000);
  const second = nextPendingSeq(1_000, first);
  assertEquals(second, -1_001);
  const later = nextPendingSeq(2_000, second);
  assertEquals(later, -2_000);
});

// The next send after a failure starts a clean turn: the fresh user
// line clears the stale red error.
Deno.test("withUserLine clears a stale error from a failed send", () => {
  const failed = sendFailed(
    withUserLine(EMPTY_TRANSCRIPT, "hello", -1, userS),
    -1,
    "post failed",
  );
  const next = withUserLine(failed, "retry", -2, userS);
  assertEquals(next.error, undefined);
  assertEquals(next.status, "busy");
});

// Correlation of an op error to the in-flight send (by req_id): a match
// closes the optimistic line via sendFailed; every mismatch falls
// through to the durable wire path. Old code had no correlation at all.
Deno.test("isInFlightError matches only the in-flight send's error", () => {
  const err = {
    kind: "error",
    req_id: 7,
    status: 409,
    message: "parked",
  } as const;
  assert(isInFlightError(err, { reqId: 7 }));
  // Different request, no in-flight send, unstamped: no match.
  assert(!isInFlightError(err, { reqId: 8 }));
  assert(!isInFlightError(err, null));
  assert(!isInFlightError(err, { reqId: undefined }));
  assert(
    !isInFlightError(
      { kind: "error", req_id: 0, status: 500, message: "unstamped" },
      { reqId: 0 },
    ),
  );
  assert(
    !isInFlightError(
      { kind: "message_accepted", req_id: 7, queue_depth: 0 },
      { reqId: 7 },
    ),
  );
});

Deno.test(
  "busy (signal) -> assistant_content (reply) -> idle (signal): append-only",
  () => {
    // busy arrives via the signal channel, not the envelope.
    let t = applySignal(EMPTY_TRANSCRIPT, { type: "busy" }, 1);
    assertEquals(t.status, "busy");
    // assistant_content arrives via the envelope (authored), paired with the
    // agent sender.
    t = applyTagmaReply(
      t,
      {
        kind: "event",
        event: { type: "assistant_content", content: "Hello." },
        history_id: 2,
        created_at: "2026-07-26T12:00:00Z",
      },
      agentP,
      2,
    );
    assertEquals(t.lines, [
      {
        historyId: 2,
        role: "assistant",
        text: "Hello.",
        sender: agentS,
        createdAt: "2026-07-26T12:00:00Z",
      },
    ]);
    // idle arrives via the signal channel.
    t = applySignal(t, { type: "idle" }, 3);
    assertEquals(t.status, "idle");
    assertEquals(t.lines, [
      {
        historyId: 2,
        role: "assistant",
        text: "Hello.",
        sender: agentS,
        createdAt: "2026-07-26T12:00:00Z",
      },
    ]);
  },
);

Deno.test(
  "multiple assistant_content lines append distinctly, then idle (signal)",
  () => {
    let t = applyTagmaReply(
      EMPTY_TRANSCRIPT,
      {
        kind: "event",
        event: { type: "assistant_content", content: "part one" },
        history_id: 1,
      },
      agentP,
      1,
    );
    t = applyTagmaReply(
      t,
      {
        kind: "event",
        event: { type: "assistant_content", content: "part two" },
        history_id: 2,
      },
      agentP,
      2,
    );
    t = applySignal(t, { type: "idle" }, 3);
    assertEquals(t.status, "idle");
    assertEquals(
      t.lines.map((l) => l.text),
      ["part one", "part two"],
    );
  },
);

Deno.test("interrupted / cancelled signals produce system lines + idle", () => {
  const intr = signal({ type: "interrupted" }, 2);
  assertEquals(intr.status, "idle");
  assertEquals(intr.lines.length, 1);
  assertEquals(intr.lines[0]!.role, "system");
  assertEquals(signal({ type: "cancelled" }).status, "idle");
});

Deno.test(
  "token_budget_exceeded / max_rounds / failover signals set error + system",
  () => {
    const tb = signal({
      type: "token_budget_exceeded",
      consumed: 9000,
      budget: 8000,
    });
    assertEquals(tb.status, "error");
    assertEquals(tb.lines[0]!.role, "system");
    assertEquals(signal({ type: "max_rounds_exceeded" }).status, "error");
    const fo = signal({
      type: "failover_chain_exhausted",
      reason: "noFailoverConfigured",
      detail: "no backups",
    });
    assertEquals(fo.status, "error");
    assertEquals(fo.error, "Model failover exhausted");
  },
);

Deno.test("error signal sets status error + a system line", () => {
  const t = signal({ type: "error", message: "boom" });
  assertEquals(t.status, "error");
  assertEquals(t.error, "boom");
  assertEquals(t.lines[0]!.role, "system");
});

Deno.test("user_message (replay echo) appends a user line + createdAt", () => {
  const t = applyTagmaReply(
    EMPTY_TRANSCRIPT,
    {
      kind: "user_message",
      history_id: 7,
      text: "hello",
      created_at: "2026-07-26T12:00:00Z",
    },
    userP,
    7,
  );
  assertEquals(t.lines, [
    {
      historyId: 7,
      role: "user",
      text: "hello",
      sender: userS,
      createdAt: "2026-07-26T12:00:00Z",
    },
  ]);
});

Deno.test(
  "withUserLine stamps a client-side createdAt + the local sender",
  () => {
    const t = withUserLine(
      EMPTY_TRANSCRIPT,
      "  hi there  ",
      -1,
      userS,
      undefined,
      new Date("2026-08-22T05:10:23.456Z"),
    );
    assertEquals(t.status, "busy");
    assertEquals(t.lines.length, 1);
    assertEquals(t.lines[0]!.historyId, -1);
    assertEquals(t.lines[0]!.role, "user");
    assertEquals(t.lines[0]!.text, "hi there");
    assertEquals(t.lines[0]!.sender, userS);
    assertEquals(t.lines[0]!.status, "sending");
    // The stamp comes from the injected clock, so the optimistic line has a
    // deterministic render time; the ack refines it via replaceLineId.
    assertEquals(t.lines[0]!.createdAt, "2026-08-22T05:10:23.456Z");
    // Empty / whitespace-only is a no-op.
    assertEquals(
      withUserLine(EMPTY_TRANSCRIPT, "   ", -2, userS),
      EMPTY_TRANSCRIPT,
    );
  },
);

Deno.test(
  "replaceLineId promotes a pending line and refines createdAt from the ack",
  () => {
    let t = withUserLine(EMPTY_TRANSCRIPT, "hi", -1, userS);
    // The ack carries the authoritative created_at; it overwrites the
    // optimistic client-side stamp. The sender survives the promotion.
    t = replaceLineId(t, -1, 42, "2026-07-26T12:00:00Z");
    assertEquals(t.lines, [
      {
        historyId: 42,
        role: "user",
        text: "hi",
        sender: userS,
        status: "sent",
        createdAt: "2026-07-26T12:00:00Z",
      },
    ]);
    // No createdAt arg -> the optimistic stamp survives the promotion.
    // Compare against t2's own stamp: two withUserLine calls stamp
    // independently, and equating them across calls flakes on ms
    // boundaries under load.
    let t2 = withUserLine(EMPTY_TRANSCRIPT, "hi", -3, userS);
    const optimistic = t2.lines[0]!.createdAt;
    t2 = replaceLineId(t2, -3, 50);
    assertEquals(t2.lines[0]!.createdAt, optimistic);
    // No-op when the pending local id is absent.
    assertEquals(replaceLineId(t, -999, 5), t);
  },
);

Deno.test(
  "markLineSent flips a sending line to sent without changing its id",
  () => {
    // The direct path has no history-id ack, so the optimistic line keeps its
    // synthetic id and only its status flips.
    let t = withUserLine(EMPTY_TRANSCRIPT, "hi", -1, userS);
    assertEquals(t.lines[0]!.status, "sending");
    t = markLineSent(t, -1);
    assertEquals(t.lines, [
      {
        historyId: -1,
        role: "user",
        text: "hi",
        sender: userS,
        status: "sent",
        createdAt: t.lines[0]!.createdAt,
      },
    ]);
    // Idempotent: re-marking a sent line is a no-op (the line is unchanged).
    const sent = t;
    assertEquals(markLineSent(sent, -1), sent);
    // No-op when the local id is absent (line already cleared on detach).
    assertEquals(markLineSent(EMPTY_TRANSCRIPT, -1), EMPTY_TRANSCRIPT);
  },
);

Deno.test(
  "cacheLineOf caches authored frames with a real history id + the sender",
  () => {
    // assistant_content with real id -> cached (createdAt + sender carried).
    assertEquals(
      cacheLineOf(
        {
          kind: "event",
          event: { type: "assistant_content", content: "hi" },
          history_id: 5,
          created_at: "2026-07-26T12:00:00Z",
        },
        agentP,
      ),
      {
        historyId: 5,
        role: "assistant",
        text: "hi",
        sender: agentS,
        createdAt: "2026-07-26T12:00:00Z",
      },
    );
    // user_message with real id -> cached (createdAt + sender carried).
    assertEquals(
      cacheLineOf(
        {
          kind: "user_message",
          history_id: 6,
          text: "q",
          created_at: "2026-07-26T12:05:00Z",
        },
        userP,
      ),
      {
        historyId: 6,
        role: "user",
        text: "q",
        sender: userS,
        createdAt: "2026-07-26T12:05:00Z",
      },
    );
    // event with no history id -> not cached (synthetic / un-stored).
    assertEquals(
      cacheLineOf(
        { kind: "event", event: { type: "assistant_content", content: "x" } },
        agentP,
      ),
      null,
    );
    // acks / batch end -> not cached.
    assertEquals(
      cacheLineOf(
        { kind: "message_accepted", req_id: 1, queue_depth: 0, history_id: 8 },
        undefined,
      ),
      null,
    );
    assertEquals(
      cacheLineOf(
        { kind: "history_batch_end", req_id: 1, count: 0, more: false },
        undefined,
      ),
      null,
    );
  },
);

// --- mergeHistoryLines + historyEntryLine (lazy-window batch merge) ---

const L = (
  historyId: number,
  role: ConversationLine["role"] = "assistant",
): ConversationLine => ({
  historyId,
  role,
  text: `m${historyId}`,
  sender: undefined,
});

Deno.test("mergeHistoryLines inserts an older batch at the head", () => {
  const state: ConversationTranscript = {
    ...EMPTY_TRANSCRIPT,
    lines: [L(10), L(20)],
  };
  const r = mergeHistoryLines(state, [L(1), L(2), L(3), L(4), L(5)]);
  assertEquals(r.added, 5);
  assertEquals(
    r.transcript.lines.map((l) => l.historyId),
    [1, 2, 3, 4, 5, 10, 20],
  );
});

Deno.test("mergeHistoryLines skips ids already rendered", () => {
  const state: ConversationTranscript = {
    ...EMPTY_TRANSCRIPT,
    lines: [L(10), L(20)],
  };
  const r = mergeHistoryLines(state, [L(5), L(10), L(15)]);
  assertEquals(r.added, 2);
  assertEquals(
    r.transcript.lines.map((l) => l.historyId),
    [5, 10, 15, 20],
  );
});

Deno.test(
  "mergeHistoryLines appends newer rows behind the optimistic tail",
  () => {
    const sending: ConversationLine = { ...L(-1, "user"), status: "sending" };
    const state: ConversationTranscript = {
      ...EMPTY_TRANSCRIPT,
      lines: [L(10), L(20), sending],
    };
    const r = mergeHistoryLines(state, [L(30)]);
    assertEquals(r.added, 1);
    assertEquals(
      r.transcript.lines.map((l) => l.historyId),
      [10, 20, 30, -1],
    );
  },
);

Deno.test("mergeHistoryLines keeps optimistic tail last on head insert", () => {
  const sending: ConversationLine = { ...L(-1, "user"), status: "sending" };
  const state: ConversationTranscript = {
    ...EMPTY_TRANSCRIPT,
    lines: [L(10), sending],
  };
  const r = mergeHistoryLines(state, [L(3), L(7)]);
  assertEquals(
    r.transcript.lines.map((l) => l.historyId),
    [3, 7, 10, -1],
  );
});

Deno.test("mergeHistoryLines: empty batch is a no-op", () => {
  const state: ConversationTranscript = { ...EMPTY_TRANSCRIPT, lines: [L(10)] };
  const r = mergeHistoryLines(state, []);
  assertEquals(r.added, 0);
  assertEquals(r.transcript, state);
});

Deno.test("mergeHistoryLines merges into an empty transcript", () => {
  const r = mergeHistoryLines(EMPTY_TRANSCRIPT, [L(1), L(2)]);
  assertEquals(r.added, 2);
  assertEquals(
    r.transcript.lines.map((l) => l.historyId),
    [1, 2],
  );
});

Deno.test(
  "mergeHistoryLines drops unstamped rows and unsorted batches still land ordered",
  () => {
    const state: ConversationTranscript = {
      ...EMPTY_TRANSCRIPT,
      lines: [L(10)],
    };
    const unstamped: ConversationLine = { ...L(0) };
    const r = mergeHistoryLines(state, [L(12), unstamped, L(4)]);
    assertEquals(r.added, 2);
    assertEquals(
      r.transcript.lines.map((l) => l.historyId),
      [4, 10, 12],
    );
  },
);

Deno.test("historyEntryLine maps event rows to assistant lines", () => {
  const line = historyEntryLine({
    sender: agentP,
    reply: {
      kind: "event",
      history_id: 7,
      event: { type: "assistant_content", content: "  hi  " },
      created_at: "2026-08-23T00:00:00Z",
    },
  });
  assertEquals(line, {
    historyId: 7,
    role: "assistant",
    text: "hi",
    sender: agentS,
    createdAt: "2026-08-23T00:00:00Z",
  });
});

Deno.test("historyEntryLine maps user_message rows to user lines", () => {
  const line = historyEntryLine({
    sender: userP,
    reply: {
      kind: "user_message",
      history_id: 8,
      text: " hello ",
      created_at: "2026-08-23T00:00:01Z",
    },
  });
  assertEquals(line, {
    historyId: 8,
    role: "user",
    text: "hello",
    sender: userS,
    createdAt: "2026-08-23T00:00:01Z",
  });
});

Deno.test("historyEntryLine skips non-content and unstamped rows", () => {
  assertEquals(
    historyEntryLine({
      sender: agentP,
      reply: {
        kind: "message_accepted",
        req_id: 1,
        queue_depth: 0,
        history_id: 9,
      },
    }),
    null,
  );
  assertEquals(
    historyEntryLine({
      sender: agentP,
      reply: {
        kind: "event",
        history_id: 0,
        event: { type: "assistant_content", content: "unstamped" },
      },
    }),
    null,
  );
  assertEquals(
    historyEntryLine({
      sender: agentP,
      reply: {
        kind: "event",
        history_id: 10,
        event: { type: "assistant_content", content: "   " },
      },
    }),
    null,
  );
});

Deno.test("attachment-only sends keep their optimistic line", () => {
  const att = { record_id: "rec-1", name: "notes.txt", size: 3 };
  const t = withUserLine(
    EMPTY_TRANSCRIPT,
    "",
    -1,
    userS,
    att,
    new Date("2026-08-22T05:10:23.456Z"),
  );
  assertEquals(t.status, "busy");
  assertEquals(t.lines, [
    {
      historyId: -1,
      role: "user",
      text: "",
      sender: userS,
      createdAt: "2026-08-22T05:10:23.456Z",
      status: "sending",
      attachment: att,
    },
  ]);
  // Whitespace-only without an attachment is still a no-op.
  assertEquals(
    withUserLine(EMPTY_TRANSCRIPT, "   ", -2, userS),
    EMPTY_TRANSCRIPT,
  );
});

Deno.test("user_message replay with an attachment and no text is kept", () => {
  const att = { record_id: "rec-2", name: "data.bin", size: 7 };
  const t = applyTagmaReply(
    EMPTY_TRANSCRIPT,
    {
      kind: "user_message",
      history_id: 9,
      text: "",
      created_at: "2026-09-01T10:00:00Z",
      attachment: att,
    },
    userP,
    9,
  );
  assertEquals(t.lines, [
    {
      historyId: 9,
      role: "user",
      text: "",
      sender: userS,
      createdAt: "2026-09-01T10:00:00Z",
      attachment: att,
    },
  ]);
});

Deno.test(
  "historyEntryLine keeps an attachment-only row, drops bare empties",
  () => {
    const att = { record_id: "rec-3", name: "a.png", size: 1 };
    const kept = historyEntryLine({
      sender: userP,
      reply: {
        kind: "user_message",
        history_id: 11,
        text: " ",
        created_at: "2026-09-01T10:00:00Z",
        attachment: att,
      },
    });
    assertEquals(kept, {
      historyId: 11,
      role: "user",
      text: "",
      sender: userS,
      createdAt: "2026-09-01T10:00:00Z",
      attachment: att,
    });
    // An empty row without an attachment stays non-content.
    assertEquals(
      historyEntryLine({
        sender: userP,
        reply: {
          kind: "user_message",
          history_id: 12,
          text: " ",
          created_at: "2026-09-01T10:00:00Z",
        },
      }),
      null,
    );
  },
);
