// Unit tests for the turn-history decision layer: what a read's answer
// carries -- state words, notice kinds, paging, truncation, reasoning
// folds, stored-text ranges, and which faces the card can render at all.
// The card picks which state to show and owns the reads; the agent-and-
// era half of a belonging check lives here with its cases, and the render
// pins cover the picking, the epoch, the still-belonging checks, the
// one-read rule and the in-flight flags; the turn-identity guard is the
// card's and has no source pin here.

import { assertEquals } from "@std/assert";
import { KallipaiError } from "@kallipai/kallipai-common";
import type {
  CallBody,
  CallRow,
  MessageRow,
  TurnRow,
  TurnsPage,
} from "@kallipai/kallipai-client";
import {
  appendRange,
  bodyView,
  callDetails,
  callStatus,
  canLoadMore,
  emptyBodyText,
  errorState,
  errorStatus,
  faceSupported,
  mergePages,
  messageView,
  pageNotices,
  reasoningView,
  stillCurrent,
  TURN_SCHEMA_VERSION,
} from "./turnView.ts";

function call(over: Partial<CallRow> = {}): CallRow {
  return {
    call_index: 0,
    call_id: "call-a",
    name: "bash",
    args: "printf hi",
    ok: true,
    truncated: false,
    ...over,
  };
}

function message(over: Partial<MessageRow> = {}): MessageRow {
  return {
    role: "assistant",
    chars: 12,
    has_reasoning: false,
    truncated: false,
    ...over,
  };
}

function turn(id: number): TurnRow {
  return { turn_id: id, at: "2026-10-27T00:00:00Z", calls: [], messages: [] };
}

function page(over: Partial<TurnsPage> = {}): TurnsPage {
  return {
    schema_version: TURN_SCHEMA_VERSION,
    rows: [],
    bad_lines: 0,
    page_truncated: false,
    ...over,
  };
}

function body(over: Partial<CallBody> = {}): CallBody {
  return {
    schema_version: TURN_SCHEMA_VERSION,
    turn_id: 1,
    call_index: 0,
    call_id: "call-a",
    name: "bash",
    offset: 0,
    total_bytes: 3,
    text: "abc",
    truncated: false,
    reclaimed: 0,
    ...over,
  };
}

Deno.test("errorState reads a status by which face asked", () => {
  assertEquals(errorState("list", 404), "no-log");
  assertEquals(errorState("detail", 404), "failed");
  assertEquals(errorState("list", 500), "failed");
  assertEquals(errorState("detail", 500), "failed");
  assertEquals(errorState("list", 200), "none");
  assertEquals(errorState("detail", undefined), "failed");
  assertEquals(errorState("list", undefined), "failed");
});

Deno.test("errorStatus takes the status off a KallipaiError only", () => {
  assertEquals(errorStatus(new TypeError("offline")), undefined);
  assertEquals(errorStatus("boom"), undefined);
  assertEquals(
    errorStatus(new KallipaiError({ status: 404, message: "no turn 3" })),
    404,
  );
});

Deno.test("pageNotices names the bad lines and the byte-cap cut", () => {
  assertEquals(pageNotices(page({ bad_lines: 3 })), [
    { kind: "bad-lines", count: 3 },
  ]);
  assertEquals(pageNotices(page({ page_truncated: true })), [
    { kind: "page-cut" },
  ]);
  assertEquals(pageNotices(page()), []);
});

Deno.test("pageNotices adds the unreadable lines of earlier pages", () => {
  assertEquals(pageNotices(page({ bad_lines: 1 }), 2), [
    { kind: "bad-lines", count: 3 },
  ]);
});

Deno.test("pageNotices stops at a newer face, whatever else it carries", () => {
  assertEquals(
    pageNotices(
      page({ schema_version: 2, bad_lines: 3, page_truncated: true }),
    ),
    [{ kind: "schema-newer" }],
  );
});

Deno.test(
  "mergePages appends older rows, keeps order and drops repeats",
  () => {
    const shown = [turn(3), turn(2)];
    assertEquals(
      mergePages(shown, page({ rows: [turn(1)] })).map((r) => r.turn_id),
      [3, 2, 1],
    );
    assertEquals(
      mergePages(shown, page({ rows: [turn(2), turn(1)] })).map(
        (r) => r.turn_id,
      ),
      [3, 2, 1],
    );
    assertEquals(mergePages(shown, page()).length, 2);
  },
);

Deno.test("canLoadMore follows the cursor, not the byte cap", () => {
  assertEquals(canLoadMore(page({ next_cursor: "abc" }), false), true);
  assertEquals(canLoadMore(page({ next_cursor: "abc" }), true), false);
  assertEquals(canLoadMore(page(), false), false);
  assertEquals(canLoadMore(page({ next_cursor: "" }), false), false);
  assertEquals(
    canLoadMore(page({ next_cursor: "abc", page_truncated: true }), false),
    true,
  );
  assertEquals(canLoadMore(null, false), false);
});

Deno.test("callStatus prefers ok over a stale approval flag", () => {
  assertEquals(callStatus(call()), "ok");
  assertEquals(
    callStatus(call({ ok: false, pending_approval: true })),
    "pending",
  );
  assertEquals(callStatus(call({ ok: false })), "failed");
  assertEquals(callStatus(call({ ok: true, pending_approval: true })), "ok");
});

Deno.test("callDetails keeps a zero exit code and complete size pairs", () => {
  assertEquals(callDetails(call({ exit_code: 0 })).exit, 0);
  assertEquals(callDetails(call()).exit, null);
  assertEquals(
    callDetails(call({ stdout_bytes: 10, stdout_lines: 2 })).stdout,
    {
      bytes: 10,
      lines: 2,
    },
  );
  assertEquals(callDetails(call({ stdout_bytes: 10 })).stdout, null);
  assertEquals(callDetails(call({ stderr_bytes: 5, stderr_lines: 1 })).stderr, {
    bytes: 5,
    lines: 1,
  });
  assertEquals(callDetails(call({ timed_out: true, truncated: true })), {
    timedOut: true,
    cut: true,
    exit: null,
    stdout: null,
    stderr: null,
    firstError: null,
  });
  assertEquals(callDetails(call({ first_error_line: "" })).firstError, null);
  assertEquals(
    callDetails(call({ first_error_line: "exit status 1" })).firstError,
    "exit status 1",
  );
});

Deno.test("messageView never offers a full read", () => {
  const cut = messageView(
    message({ chars: 9000, truncated: true, text: "head … tail" }),
  );
  assertEquals(cut.cut, true);
  assertEquals(cut.text, "head … tail");
  assertEquals(cut.chars, 9000);
  assertEquals(cut.canReadFull, false);
  const plain = messageView(message({ truncated: false }));
  assertEquals(plain.cut, false);
  assertEquals(plain.text, null);
});

Deno.test("reasoningView tells none, text and unavailable apart", () => {
  assertEquals(reasoningView(message({ has_reasoning: false })), {
    state: "none",
    text: null,
    cut: false,
  });
  assertEquals(
    reasoningView(
      message({
        has_reasoning: true,
        reasoning: "because",
        reasoning_truncated: true,
      }),
    ),
    { state: "text", text: "because", cut: true },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: "because" })),
    { state: "text", text: "because", cut: false },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: " \n\t " })),
    { state: "unavailable", text: null, cut: false },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: " because " })),
    { state: "text", text: " because ", cut: false },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: "one\n\ntwo" })),
    { state: "text", text: "one\n\ntwo", cut: false },
  );
  assertEquals(reasoningView(message({ has_reasoning: true })), {
    state: "unavailable",
    text: null,
    cut: false,
  });
});

Deno.test("bodyView measures the range it received, in bytes", () => {
  assertEquals(bodyView(body()), {
    from: 0,
    to: 3,
    total: 3,
    more: null,
    atEnd: true,
    empty: false,
    reclaimed: 0,
  });
  assertEquals(
    bodyView(
      body({
        offset: 16384,
        text: "xyz",
        truncated: true,
        next_offset: 16387,
        total_bytes: 20000,
      }),
    ),
    {
      from: 16384,
      to: 16387,
      total: 20000,
      more: 16387,
      atEnd: false,
      empty: false,
      reclaimed: 0,
    },
  );
  const stuck = bodyView(body({ truncated: true }));
  assertEquals(stuck.more, null);
  assertEquals(stuck.atEnd, false);
  const empty = bodyView(body({ text: "", total_bytes: 0 }));
  assertEquals(empty.empty, true);
  assertEquals(empty.atEnd, true);
  assertEquals(bodyView(body({ reclaimed: 2 })).reclaimed, 2);
});

Deno.test("faceSupported stops at a face this build does not know", () => {
  assertEquals(faceSupported(1), true);
  assertEquals(faceSupported(TURN_SCHEMA_VERSION + 1), false);
});

Deno.test("stillCurrent drops a read whose agent or era moved on", () => {
  const asked = { agent: "agent-a", epoch: 2 };
  assertEquals(stillCurrent(asked, { agent: "agent-a", epoch: 2 }), true);
  assertEquals(stillCurrent(asked, { agent: "agent-b", epoch: 2 }), false);
  assertEquals(stillCurrent(asked, { agent: "agent-a", epoch: 3 }), false);
});

Deno.test("appendRange keeps the first origin and glues the text on", () => {
  const head = appendRange(
    null,
    body({
      offset: 100,
      text: "ab",
      truncated: true,
      next_offset: 102,
      total_bytes: 200,
    }),
  );
  assertEquals(head.from, 100);
  assertEquals(head.atEnd, false);
  const tail = appendRange(
    head,
    body({ offset: 102, text: "cd", total_bytes: 200 }),
  );
  assertEquals(tail.from, 100);
  assertEquals(tail.to, 104);
  assertEquals(tail.text, "abcd");
  assertEquals(tail.more, null);
  assertEquals(tail.atEnd, true);
  assertEquals(appendRange(emptyBodyText(), body()).text, "abc");
});

Deno.test(
  "appendRange calls a block empty only when every range was empty",
  () => {
    const blank = appendRange(null, body({ text: "", total_bytes: 0 }));
    assertEquals(blank.empty, true);
    const still = appendRange(blank, body({ text: "", total_bytes: 0 }));
    assertEquals(still.empty, true);
    const filled = appendRange(
      still,
      body({ offset: 0, text: "x", total_bytes: 1 }),
    );
    assertEquals(filled.empty, false);
    const after = appendRange(
      filled,
      body({
        offset: 1,
        text: "",
        truncated: true,
        next_offset: 2,
        total_bytes: 3,
      }),
    );
    assertEquals(after.empty, false);
  },
);
