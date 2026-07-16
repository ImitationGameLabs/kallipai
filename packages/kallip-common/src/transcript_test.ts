import { assertEquals } from "@std/assert";
import { applyEvent, EMPTY_TRANSCRIPT, withUserLine } from "./transcript.ts";
import { isBoundary } from "./event.ts";
import type { DomainEvent } from "./event.ts";
import type { TranscriptLine, TranscriptState } from "./transcript.ts";

function lastLine(state: TranscriptState): TranscriptLine | undefined {
  return state.lines[state.lines.length - 1];
}

Deno.test("assistantContentDelta appends to a streaming assistant line", () => {
  let s = EMPTY_TRANSCRIPT;
  s = applyEvent(s, { type: "busy" });
  s = applyEvent(s, { type: "assistantContentDelta", delta: "Hello" });
  s = applyEvent(s, { type: "assistantContentDelta", delta: ", world" });
  const last = lastLine(s);
  assertEquals(last?.kind, "assistant");
  assertEquals((last as { text: string }).text, "Hello, world");
  assertEquals((last as { streaming?: boolean }).streaming, true);
  assertEquals(s.agentBusy, true);
});

Deno.test(
  "finished finalizes the trailing streaming line and clears busy",
  () => {
    let s = EMPTY_TRANSCRIPT;
    s = applyEvent(s, {
      type: "assistantContentDelta",
      delta: "streaming text",
    });
    s = applyEvent(s, { type: "finished", content: "streaming text" });
    const last = lastLine(s);
    assertEquals(last?.kind, "assistant");
    assertEquals((last as { text: string }).text, "streaming text");
    assertEquals((last as { streaming?: boolean }).streaming, false);
    assertEquals(s.agentBusy, false);
  },
);

Deno.test(
  "finished without prior deltas pushes the full content (agora path)",
  () => {
    let s = EMPTY_TRANSCRIPT;
    s = applyEvent(s, { type: "finished", content: "a full reply" });
    const last = lastLine(s);
    assertEquals(last?.kind, "assistant");
    assertEquals((last as { text: string }).text, "a full reply");
    assertEquals(s.lines.length, 1);
  },
);

Deno.test("toolCall finalizes any trailing streaming content", () => {
  let s = EMPTY_TRANSCRIPT;
  s = applyEvent(s, { type: "assistantContentDelta", delta: "partial" });
  s = applyEvent(s, {
    type: "toolCall",
    name: "bash_exec",
    args: '{"cmd":"ls"}',
  });
  // The streaming assistant line is finalized (streaming=false), toolCall pushed.
  assertEquals(s.lines.length, 2);
  assertEquals((s.lines[0] as { streaming?: boolean }).streaming, false);
  assertEquals(s.lines[1]?.kind, "toolCall");
});

Deno.test("streamReset finalizes trailing streaming and drops a line", () => {
  let s = EMPTY_TRANSCRIPT;
  s = applyEvent(s, { type: "reasoningDelta", delta: "thinking..." });
  s = applyEvent(s, {
    type: "streamReset",
    error: "upstream dropped",
    attempt: 1,
    maxAttempts: 3,
    delaySecs: 0.5,
  });
  assertEquals(s.lines.length, 2);
  assertEquals((s.lines[0] as { streaming?: boolean }).streaming, false);
  assertEquals(s.lines[1]?.kind, "streamDropped");
});

Deno.test("error and terminal events clear agentBusy", () => {
  for (const ev of [
    { type: "error", message: "boom" },
    { type: "interrupted" },
    { type: "cancelled" },
    { type: "maxRoundsExceeded" },
    { type: "tokenBudgetExceeded", consumed: 100, budget: 50 },
  ] as DomainEvent[]) {
    let s = applyEvent(EMPTY_TRANSCRIPT, { type: "busy" });
    s = applyEvent(s, ev);
    assertEquals(s.agentBusy, false, `expected busy cleared for ${ev.type}`);
  }
});

Deno.test("approvalUpdated does not change the transcript", () => {
  let s = EMPTY_TRANSCRIPT;
  s = applyEvent(s, { type: "busy" });
  s = applyEvent(s, { type: "approvalUpdated", id: "a1", status: "committed" });
  assertEquals(s.lines.length, 0);
  assertEquals(s.agentBusy, true);
});

Deno.test("withUserLine appends a user line", () => {
  const s = withUserLine(EMPTY_TRANSCRIPT, "hello");
  assertEquals(s.lines.length, 1);
  assertEquals(s.lines[0]?.kind, "user");
});

Deno.test("isBoundary matches the turn-boundary set", () => {
  const boundary: DomainEvent["type"][] = [
    "toolCall",
    "finished",
    "cancelled",
    "interrupted",
    "error",
    "maxRoundsExceeded",
    "failoverChainExhausted",
    "tokenBudgetExceeded",
  ];
  for (const type of boundary) {
    const ev = { type } as DomainEvent;
    assertEquals(isBoundary(ev), true, `expected ${type} to be a boundary`);
  }
  assertEquals(isBoundary({ type: "status", message: "x" }), false);
  assertEquals(isBoundary({ type: "busy" }), false);
});
