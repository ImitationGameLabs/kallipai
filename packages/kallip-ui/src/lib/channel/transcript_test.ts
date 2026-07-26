// Tests for the online transcript reducer: each TagmaReply / TagmaEvent variant
// maps to the expected lines + status, assistant content is append-only (no
// streaming merge), and `idle` is a content-less status transition.

import { assertEquals } from "@std/assert";
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
      },
      2,
    );
    assertEquals(t.lines, [
      {
        historyId: 2,
        role: "assistant",
        text: "Hello.",
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
    [{ historyId: 1, role: "system", text: "thinking" }],
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

Deno.test("user_message (replay echo) appends a user line", () => {
  const t = applyTagmaReply(
    EMPTY_TRANSCRIPT,
    { kind: "user_message", history_id: 7, text: "hello" },
    7,
  );
  assertEquals(t.lines, [{ historyId: 7, role: "user", text: "hello" }]);
});

Deno.test("withUserLine appends a pending user line and flips to busy", () => {
  const t = withUserLine(EMPTY_TRANSCRIPT, "  hi there  ", -1);
  assertEquals(t.status, "busy");
  assertEquals(t.lines, [{ historyId: -1, role: "user", text: "hi there" }]);
  // Empty / whitespace-only is a no-op.
  assertEquals(withUserLine(EMPTY_TRANSCRIPT, "   ", -2), EMPTY_TRANSCRIPT);
});

Deno.test(
  "replaceLineId promotes a pending line to its real history id",
  () => {
    let t = withUserLine(EMPTY_TRANSCRIPT, "hi", -1);
    t = replaceLineId(t, -1, 42);
    assertEquals(t.lines, [{ historyId: 42, role: "user", text: "hi" }]);
    // No-op when the pending local id is absent.
    assertEquals(replaceLineId(t, -999, 5), t);
  },
);

Deno.test(
  "cacheLineOf caches content frames with a real history id only",
  () => {
    // assistant_content with real id -> cached.
    assertEquals(
      cacheLineOf({
        kind: "event",
        event: { type: "assistant_content", content: "hi" },
        history_id: 5,
      }),
      { historyId: 5, role: "assistant", text: "hi" },
    );
    // user_message with real id -> cached.
    assertEquals(
      cacheLineOf({ kind: "user_message", history_id: 6, text: "q" }),
      {
        historyId: 6,
        role: "user",
        text: "q",
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
