// Tests for the conversation transcript reducer. Authored content (assistant
// messages) arrives via applyTagmaReply; runtime signals (busy/idle, terminals,
// errors) arrives via applySignal. Each variant maps to the expected lines +
// status; assistant content is append-only (no streaming merge); busy/idle are
// content-less status transitions.

import { assert, assertEquals } from "@std/assert";
import {
  applySignal,
  applyTagmaReply,
  cacheLineOf,
  type ConversationTranscript,
  EMPTY_TRANSCRIPT,
  markLineSent,
  replaceLineId,
  withUserLine,
} from "./transcript.ts";
import type { SignalEvent, TagmaReply } from "@kallipai/kallip-lesche-client";

function reply(r: TagmaReply, lineId = 1): ConversationTranscript {
  return applyTagmaReply(EMPTY_TRANSCRIPT, r, lineId);
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
        {
          kind: "message_accepted",
          req_id: 1,
          queue_depth: 0,
        },
        1,
      ).lines.length,
      0,
    );
    assertEquals(
      applyTagmaReply(EMPTY_TRANSCRIPT, { kind: "interrupted", req_id: 1 }, 1)
        .lines.length,
      0,
    );
    assertEquals(
      applyTagmaReply(
        EMPTY_TRANSCRIPT,
        { kind: "history_batch_end", req_id: 1, count: 0, more: false },
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
      createdAt: undefined,
    },
  ]);
});

Deno.test(
  "busy (signal) -> assistant_content (reply) -> idle (signal): append-only",
  () => {
    // busy arrives via the signal channel, not the envelope.
    let t = applySignal(EMPTY_TRANSCRIPT, { type: "busy" }, 1);
    assertEquals(t.status, "busy");
    // assistant_content arrives via the envelope (authored).
    t = applyTagmaReply(
      t,
      {
        kind: "event",
        event: { type: "assistant_content", content: "Hello." },
        history_id: 2,
        created_at: "2026-07-26T12:00:00Z",
      },
      2,
    );
    assertEquals(t.lines, [
      {
        historyId: 2,
        role: "assistant",
        text: "Hello.",
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
      1,
    );
    t = applyTagmaReply(
      t,
      {
        kind: "event",
        event: { type: "assistant_content", content: "part two" },
        history_id: 2,
      },
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
    7,
  );
  assertEquals(t.lines, [
    {
      historyId: 7,
      role: "user",
      text: "hello",
      createdAt: "2026-07-26T12:00:00Z",
    },
  ]);
});

Deno.test("withUserLine stamps a client-side createdAt", () => {
  const t = withUserLine(EMPTY_TRANSCRIPT, "  hi there  ", -1);
  assertEquals(t.status, "busy");
  assertEquals(t.lines.length, 1);
  assertEquals(t.lines[0]!.historyId, -1);
  assertEquals(t.lines[0]!.role, "user");
  assertEquals(t.lines[0]!.text, "hi there");
  assertEquals(t.lines[0]!.status, "sending");
  // A client-side ISO stamp is present so the optimistic line shows a time
  // immediately; the ack refines it via replaceLineId.
  assertEquals(typeof t.lines[0]!.createdAt, "string");
  assert(t.lines[0]!.createdAt!.length > 0);
  // Empty / whitespace-only is a no-op.
  assertEquals(withUserLine(EMPTY_TRANSCRIPT, "   ", -2), EMPTY_TRANSCRIPT);
});

Deno.test(
  "replaceLineId promotes a pending line and refines createdAt from the ack",
  () => {
    let t = withUserLine(EMPTY_TRANSCRIPT, "hi", -1);
    const optimistic = t.lines[0]!.createdAt;
    // The ack carries the authoritative created_at; it overwrites the
    // optimistic client-side stamp.
    t = replaceLineId(t, -1, 42, "2026-07-26T12:00:00Z");
    assertEquals(t.lines, [
      {
        historyId: 42,
        role: "user",
        text: "hi",
        status: "sent",
        createdAt: "2026-07-26T12:00:00Z",
      },
    ]);
    // No createdAt arg -> the optimistic stamp survives the promotion.
    let t2 = withUserLine(EMPTY_TRANSCRIPT, "hi", -3);
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
    let t = withUserLine(EMPTY_TRANSCRIPT, "hi", -1);
    assertEquals(t.lines[0]!.status, "sending");
    t = markLineSent(t, -1);
    assertEquals(t.lines, [
      {
        historyId: -1,
        role: "user",
        text: "hi",
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
  "cacheLineOf caches authored frames with a real history id only",
  () => {
    // assistant_content with real id -> cached (createdAt carried through).
    assertEquals(
      cacheLineOf({
        kind: "event",
        event: { type: "assistant_content", content: "hi" },
        history_id: 5,
        created_at: "2026-07-26T12:00:00Z",
      }),
      {
        historyId: 5,
        role: "assistant",
        text: "hi",
        createdAt: "2026-07-26T12:00:00Z",
      },
    );
    // user_message with real id -> cached (createdAt carried through).
    assertEquals(
      cacheLineOf({
        kind: "user_message",
        history_id: 6,
        text: "q",
        created_at: "2026-07-26T12:05:00Z",
      }),
      {
        historyId: 6,
        role: "user",
        text: "q",
        createdAt: "2026-07-26T12:05:00Z",
      },
    );
    // event with no history id -> not cached (synthetic / un-stored).
    assertEquals(
      cacheLineOf({
        kind: "event",
        event: { type: "assistant_content", content: "x" },
      }),
      null,
    );
    // acks / batch end -> not cached.
    assertEquals(
      cacheLineOf({
        kind: "message_accepted",
        req_id: 1,
        queue_depth: 0,
        history_id: 8,
      }),
      null,
    );
    assertEquals(
      cacheLineOf({
        kind: "history_batch_end",
        req_id: 1,
        count: 0,
        more: false,
      }),
      null,
    );
  },
);
