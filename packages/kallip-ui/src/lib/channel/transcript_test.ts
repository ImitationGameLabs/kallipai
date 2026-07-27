// Tests for the online transcript reducer: each TagmaReply / TagmaEvent variant
// maps to the expected lines + status, assistant content is append-only (no
// streaming merge), and `idle` is a content-less status transition.

import { assert, assertEquals } from "@std/assert";
import {
  applyTagmaReply,
  cacheLineOf,
  type ChannelTranscript,
  EMPTY_TRANSCRIPT,
  replaceLineId,
  withUserLine,
} from "./transcript.ts";
import type { TagmaReply } from "@kallipai/kallip-lesche-client";

function reply(r: TagmaReply, lineId = 1): ChannelTranscript {
  return applyTagmaReply(EMPTY_TRANSCRIPT, r, lineId);
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
  "busy -> assistant_content -> idle: append-only, idle at yield",
  () => {
    let t = applyTagmaReply(
      EMPTY_TRANSCRIPT,
      {
        kind: "event",
        event: { type: "busy" },
      },
      1,
    );
    assertEquals(t.status, "busy");
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
    t = applyTagmaReply(t, { kind: "event", event: { type: "idle" } }, 3);
    // idle is content-less: just transition, no duplicate line.
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
  "multiple assistant_content lines append distinctly, then idle",
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
    t = applyTagmaReply(t, { kind: "event", event: { type: "idle" } }, 3);
    assertEquals(t.status, "idle");
    assertEquals(
      t.lines.map((l) => l.text),
      ["part one", "part two"],
    );
  },
);

Deno.test("status / interrupted / cancelled produce system lines", () => {
  assertEquals(
    reply({
      kind: "event",
      event: { type: "status", message: "thinking" },
      history_id: 1,
    }).lines,
    [{ historyId: 1, role: "system", text: "thinking", createdAt: undefined }],
  );
  const intr = reply(
    {
      kind: "event",
      event: { type: "interrupted" },
      history_id: 2,
    },
    2,
  );
  assertEquals(intr.status, "idle");
  assertEquals(intr.lines.length, 1);
  assertEquals(intr.lines[0]!.role, "system");
  assertEquals(
    reply({ kind: "event", event: { type: "cancelled" } }).status,
    "idle",
  );
});

Deno.test(
  "token_budget_exceeded / max_rounds / failover set error + system",
  () => {
    const tb = reply({
      kind: "event",
      event: { type: "token_budget_exceeded", consumed: 9000, budget: 8000 },
      history_id: 1,
    });
    assertEquals(tb.status, "error");
    assertEquals(tb.lines[0]!.role, "system");
    assertEquals(
      reply({ kind: "event", event: { type: "max_rounds_exceeded" } }).status,
      "error",
    );
    const fo = reply({
      kind: "event",
      event: {
        type: "failover_chain_exhausted",
        reason: "noFailoverConfigured",
        detail: "no backups",
      },
    });
    assertEquals(fo.status, "error");
    assertEquals(fo.error, "Model failover exhausted");
  },
);

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
  "cacheLineOf caches content frames with a real history id only",
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
    // status-only event (busy) -> not cached.
    assertEquals(
      cacheLineOf({
        kind: "event",
        event: { type: "busy" },
        history_id: 7,
      }),
      null,
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
