// Tests for the online transcript reducer: each TagmaReply / TagmaEvent variant
// maps to the expected lines + status, assistant content is append-only (no
// streaming merge), and Finished de-duplicates a trailing identical line.

import { assertEquals } from "@std/assert";
import {
  applyTagmaReply,
  EMPTY_TRANSCRIPT,
  withUserLine,
  type ChannelTranscript,
} from "./transcript.ts";
import type { TagmaReply } from "@kallipai/kallip-agora-client";

function reply(r: TagmaReply): ChannelTranscript {
  return applyTagmaReply(EMPTY_TRANSCRIPT, r);
}

Deno.test("message_accepted and interrupted acks are no-ops", () => {
  assertEquals(
    applyTagmaReply(EMPTY_TRANSCRIPT, {
      kind: "message_accepted",
      req_id: 1,
      queue_depth: 0,
    }).lines.length,
    0,
  );
  assertEquals(
    applyTagmaReply(EMPTY_TRANSCRIPT, { kind: "interrupted", req_id: 1 }).lines
      .length,
    0,
  );
});

Deno.test("TagmaReply error sets status error + a system line", () => {
  const t = reply({
    kind: "error",
    req_id: 2,
    status: 502,
    message: "herald blew up",
  });
  assertEquals(t.status, "error");
  assertEquals(t.error, "herald blew up");
  assertEquals(t.lines, [{ seq: 0, role: "system", text: "herald blew up" }]);
});

Deno.test(
  "busy -> assistant_content -> finished: append-only, idle at finish",
  () => {
    let t = applyTagmaReply(EMPTY_TRANSCRIPT, {
      kind: "event",
      event: { type: "busy" },
    });
    assertEquals(t.status, "busy");
    t = applyTagmaReply(t, {
      kind: "event",
      event: { type: "assistant_content", content: "Hello." },
    });
    assertEquals(t.lines, [{ seq: 0, role: "assistant", text: "Hello." }]);
    t = applyTagmaReply(t, {
      kind: "event",
      event: { type: "finished", content: "Hello." },
    });
    // Trailing identical assistant line -> just go idle, no duplicate.
    assertEquals(t.status, "idle");
    assertEquals(t.lines, [{ seq: 0, role: "assistant", text: "Hello." }]);
  },
);

Deno.test("finished with new content appends a distinct line", () => {
  let t = applyTagmaReply(EMPTY_TRANSCRIPT, {
    kind: "event",
    event: { type: "assistant_content", content: "part one" },
  });
  t = applyTagmaReply(t, {
    kind: "event",
    event: { type: "finished", content: "part two" },
  });
  assertEquals(t.status, "idle");
  assertEquals(
    t.lines.map((l) => l.text),
    ["part one", "part two"],
  );
});

Deno.test("status / interrupted / cancelled produce system lines", () => {
  assertEquals(
    reply({
      kind: "event",
      event: { type: "status", message: "thinking" },
    }).lines,
    [{ seq: 0, role: "system", text: "thinking" }],
  );
  const intr = reply({ kind: "event", event: { type: "interrupted" } });
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

Deno.test("withUserLine appends a user line and flips to busy", () => {
  const t = withUserLine(EMPTY_TRANSCRIPT, "  hi there  ");
  assertEquals(t.status, "busy");
  assertEquals(t.lines, [{ seq: 0, role: "user", text: "hi there" }]);
  // Empty / whitespace-only is a no-op.
  assertEquals(withUserLine(EMPTY_TRANSCRIPT, "   "), EMPTY_TRANSCRIPT);
});
