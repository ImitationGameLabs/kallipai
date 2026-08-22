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
  type ConversationTranscript,
  EMPTY_TRANSCRIPT,
  markLineSent,
  replaceLineId,
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
    const optimistic = t.lines[0]!.createdAt;
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
    let t2 = withUserLine(EMPTY_TRANSCRIPT, "hi", -3, userS);
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
