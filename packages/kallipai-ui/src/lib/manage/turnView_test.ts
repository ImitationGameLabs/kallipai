// Unit tests for the turn-history decision layer: what a read's answer
// carries -- state words, notice kinds, paging, truncation, reasoning
// folds, stored-text ranges, and which faces the page can render at all.
// The page picks which state to show and owns the reads; both halves of a
// belonging check live here with their cases, and the render pins cover
// the picking, the epoch, the still-belonging checks, the one-read rule
// and the in-flight flags.

import { assertEquals, assertExists } from "@std/assert";
import { KallipaiError } from "@kallipai/kallipai-common";
import type {
  CallBody,
  CallRow,
  MessageRow,
  TurnRow,
  TurnsPage,
} from "@kallipai/kallipai-client";
import {
  argsView,
  AUTO_READ_CALLS,
  bodyStillMine,
  bodyKey,
  type BodyEntry,
  appendRange,
  bodyView,
  callDetails,
  callStatus,
  canLoadMore,
  emptyBodyText,
  errorState,
  errorStatus,
  faceSupported,
  finishRead,
  mergePages,
  messageView,
  NO_TURN,
  pageNotices,
  prefixBytes,
  reasoningView,
  reasoningKey,
  type ShownTurn,
  showOnRows,
  showOnWidth,
  stillCurrent,
  startRead,
  wholeText,
  HISTORY_SCHEMA_VERSION,
  toneDot,
  untouchedResults,
  turnShape,
  visibleMessages,
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
    ...over,
  };
}

function turn(id: number): TurnRow {
  return { turn_id: id, at: "2026-10-27T00:00:00Z", calls: [], messages: [] };
}

function page(over: Partial<TurnsPage> = {}): TurnsPage {
  return {
    schema_version: HISTORY_SCHEMA_VERSION,
    rows: [],
    bad_lines: 0,
    page_truncated: false,
    ...over,
  };
}

function body(over: Partial<CallBody> = {}): CallBody {
  return {
    schema_version: HISTORY_SCHEMA_VERSION,
    turn_id: 1,
    call_index: 0,
    call_id: "call-a",
    name: "bash",
    part: "result",
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

Deno.test("pageNotices stops at another face, and names it", () => {
  assertEquals(
    pageNotices(
      page({
        schema_version: HISTORY_SCHEMA_VERSION + 1,
        bad_lines: 3,
        page_truncated: true,
      }),
    ),
    [{ kind: "face", face: HISTORY_SCHEMA_VERSION + 1 }],
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

Deno.test(
  "callStatus: an exit code outranks the envelope, which outranks a stale flag",
  () => {
    assertEquals(callStatus(call()), "ok");
    assertEquals(
      callStatus(call({ ok: true, exit_code: 2 })),
      "failed",
      "a non-zero exit is a failure even inside a successful envelope",
    );
    assertEquals(
      callStatus(call({ ok: true, exit_code: 0 })),
      "ok",
      "a zero exit leaves the envelope's answer alone",
    );
    assertEquals(
      callStatus(call({ ok: true })),
      "ok",
      "no exit code is no verdict to overrule the envelope with",
    );
    assertEquals(
      callStatus(call({ ok: false, pending_approval: true })),
      "pending",
    );
    assertEquals(callStatus(call({ ok: false })), "failed");
    assertEquals(callStatus(call({ ok: true, pending_approval: true })), "ok");
  },
);

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
    callDetails(
      call({
        first_error_line: "exit status 1",
        error_more: true,
      }),
    ).firstError,
    { text: "exit status 1", more: true },
  );
  // Later lines are a rest this row does not carry, so one bit covers them:
  // the first line fits the cap and the block still has a read to offer.
  assertEquals(
    callDetails(call({ first_error_line: "stderr:", error_lines: 12 }))
      .firstError,
    { text: "stderr:", more: true },
  );
  assertEquals(callDetails(call({ first_error_line: "crashed" })).firstError, {
    text: "crashed",
    more: false,
  });
});

Deno.test("prefixBytes measures the drawn text the way the face pages", () => {
  assertEquals(prefixBytes(null), 0);
  assertEquals(prefixBytes(""), 0);
  // A character outside ASCII counts by its bytes, not its units.
  assertEquals(prefixBytes("é"), 2);
  assertEquals(prefixBytes("😀"), 4);
});

Deno.test("a read from the prefix's end joins back to the whole text", () => {
  const whole = "héllo 😀 world";
  const drawn = whole.slice(0, 8);
  const rest = whole.slice(8);
  const read = appendRange(
    null,
    body({ offset: prefixBytes(drawn), text: rest }),
  );
  assertExists(read);
  assertEquals(read.from, prefixBytes(drawn));
  assertEquals(wholeText(drawn, read), whole);
});

Deno.test("wholeText puts the prefix back in front of the range read", () => {
  // What the copy control hands over is what the box draws: the prefix the
  // face sent and the range that continued it, in that order.
  assertEquals(wholeText("head ", { text: "rest" }), "head rest");
  assertEquals(wholeText(null, { text: "rest" }), "rest");
  assertEquals(wholeText("head", undefined), "head");
  // A read that has only reached the middle of the rest: the prefix plus
  // what came back, never the range alone.
  const whole = "aaaaaaaaaaBBBBBBBBBB";
  const drawn = whole.slice(0, 10);
  const partial = appendRange(
    null,
    body({
      offset: prefixBytes(drawn),
      text: "BBBBB",
      truncated: true,
      next_offset: 15,
    }),
  );
  assertExists(partial);
  assertEquals(wholeText(drawn, partial), "aaaaaaaaaaBBBBB");
});

Deno.test(
  "messageView holds a text and says whether the rest is a read",
  () => {
    const read = messageView(
      message({ chars: 9000, text: "head", text_more: true }),
    );
    assertEquals(read, { text: "head", more: true });
    const plain = messageView(message({ text: "all of it" }));
    assertEquals(plain, { text: "all of it", more: false });
    // A page carries no text at all: the text is absent, not empty.
    assertEquals(messageView(message()), { text: null, more: false });
  },
);

Deno.test("reasoningView tells none, text and unavailable apart", () => {
  const none = { text: null, more: false };
  assertEquals(reasoningView(message({ has_reasoning: false })), {
    state: "none",
    ...none,
  });
  assertEquals(
    reasoningView(
      message({
        has_reasoning: true,
        reasoning: "because",
        reasoning_more: true,
      }),
    ),
    { state: "text", text: "because", more: true },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: "because" })),
    { state: "text", text: "because", more: false },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: " \n\t " })),
    { state: "unavailable", ...none },
  );
  // A blank text with no rest is nothing on screen, whatever the count.
  assertEquals(reasoningView(message({ has_reasoning: true, reasoning: "" })), {
    state: "unavailable",
    ...none,
  });
  // A blank prefix is a prefix: the rest is one read away, so it is drawn
  // (as blanks) with the control that reads on.
  assertEquals(
    reasoningView(
      message({ has_reasoning: true, reasoning: "  ", reasoning_more: true }),
    ),
    { state: "text", text: "  ", more: true },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: " because " })),
    { state: "text", text: " because ", more: false },
  );
  assertEquals(
    reasoningView(message({ has_reasoning: true, reasoning: "one\n\ntwo" })),
    { state: "text", text: "one\n\ntwo", more: false },
  );
  assertEquals(reasoningView(message({ has_reasoning: true })), {
    state: "unavailable",
    ...none,
  });
});

Deno.test("turnShape names the turn, and a call wins over the roles", () => {
  assertEquals(
    turnShape({ ...turn(1), messages: [message({ role: "user", chars: 3 })] }),
    "user",
  );
  assertEquals(
    turnShape({ ...turn(2), messages: [message({ role: "assistant" })] }),
    "assistant",
  );
  assertEquals(
    turnShape({ ...turn(3), calls: [call()], messages: [message()] }),
    "tools",
  );
  assertEquals(
    turnShape({
      ...turn(4),
      calls: [call(), call({ call_index: 1 })],
      messages: [message()],
    }),
    "tools",
  );
  assertEquals(
    turnShape({
      ...turn(5),
      calls: [call()],
      messages: [message({ role: "user", chars: 4 })],
    }),
    "tools",
  );
  assertEquals(turnShape(turn(6)), "none");
  assertEquals(
    turnShape({ ...turn(7), messages: [message({ role: "system" })] }),
    "none",
  );
});

Deno.test(
  "a tool round still shows what its model said around the call",
  () => {
    const messages = [
      message({ role: "assistant", chars: 21, text: "checking the log" }),
      message({ role: "tool", chars: 91, text: "waiting until asked" }),
    ];
    const row = { ...turn(8), calls: [call()], messages };
    assertEquals(turnShape(row), "tools");
    assertEquals(visibleMessages(row), [{ index: 0, message: messages[0] }]);
  },
);

Deno.test("visibleMessages drops what a call view already draws", () => {
  const messages = [
    message({ role: "user", chars: 0 }),
    message({ role: "tool", chars: 91, text: "waiting until asked" }),
    message({ role: "assistant", chars: 0 }),
    message({ role: "assistant", chars: 4, text: "done" }),
    message({
      role: "assistant",
      chars: 0,
      has_reasoning: true,
      reasoning: "because",
    }),
    message({
      role: "assistant",
      chars: 0,
      has_reasoning: true,
      reasoning: " \n\t ",
    }),
    message({ role: "system", chars: 5, text: "rule" }),
  ];
  const row = { ...turn(9), calls: [call()], messages };
  assertEquals(
    visibleMessages(row).map((slot) => slot.index),
    [0, 3, 4, 6],
  );
});

Deno.test(
  "visibleMessages keeps every message where no call view draws them",
  () => {
    const messages = [
      message({ role: "tool", chars: 91, text: "waiting until asked" }),
      message({ role: "assistant", chars: 4, text: "done" }),
    ];
    assertEquals(
      visibleMessages({ ...turn(10), messages }).map((slot) => slot.index),
      [0, 1],
    );
  },
);

Deno.test("a role whose block draws nothing is no shape at all", () => {
  // One assistant message with neither text nor reasoning: the pane would
  // be empty, so the row says nothing instead of naming a shape.
  const row = {
    ...turn(11),
    messages: [message({ role: "assistant", chars: 0 })],
  };
  assertEquals(visibleMessages(row), []);
  assertEquals(turnShape(row), "none");
});

Deno.test("a blank text is no text, and a page still answers by count", () => {
  const blank = {
    ...turn(12),
    messages: [message({ chars: 5, text: " \n\t " })],
  };
  assertEquals(visibleMessages(blank), []);
  assertEquals(turnShape(blank), "none");
  // A page carries shape only: no text to read, so the count is the evidence.
  const pageShaped = { ...turn(13), messages: [message({ chars: 5 })] };
  assertEquals(
    visibleMessages(pageShaped).map((slot) => slot.index),
    [0],
  );
  assertEquals(turnShape(pageShaped), "assistant");
  // A blank prefix with a rest is not nothing: dropping it would hide a
  // message the record holds and can hand over.
  const blankPrefix = {
    ...turn(14),
    messages: [message({ chars: 5, text: " ", text_more: true })],
  };
  assertEquals(
    visibleMessages(blankPrefix).map((slot) => slot.index),
    [0],
  );
  assertEquals(turnShape(blankPrefix), "assistant");
});

Deno.test("a reply with reasoning and no text reads twice", () => {
  // The detail face is sent the reasoning and a list page is not, so this
  // shape is where the two faces answer opposite ways: the pane draws a
  // block, the row has no word for it.
  const detailed = {
    ...turn(15),
    messages: [
      message({
        role: "assistant",
        chars: 0,
        has_reasoning: true,
        reasoning: "because",
      }),
    ],
  };
  assertEquals(
    visibleMessages(detailed).map((slot) => slot.index),
    [0],
  );
  assertEquals(turnShape(detailed), "assistant");
  // The page face sees the flag and no text: a block it cannot draw.
  const pageShaped = {
    ...turn(16),
    messages: [message({ role: "assistant", chars: 0, has_reasoning: true })],
  };
  assertEquals(visibleMessages(pageShaped), []);
  assertEquals(turnShape(pageShaped), "none");
});

Deno.test("the bodies a call row cannot claim are the ones lost", () => {
  // Each call view carries the tool messages of its own call. A record with
  // three tool messages and one call has two bodies no view draws; this
  // pins that the pane neither throws nor pretends to show them.
  const row = {
    ...turn(14),
    calls: [call()],
    messages: [
      message({ role: "tool", chars: 3, text: "one" }),
      message({ role: "tool", chars: 3, text: "two" }),
      message({ role: "tool", chars: 3, text: "three" }),
    ],
  };
  assertEquals(visibleMessages(row), []);
  assertEquals(turnShape(row), "tools");
});

Deno.test(
  "a message with no text keeps its block when it is not a reply",
  () => {
    // An image-only user body carries no text. No record here has that shape,
    // and the pane would draw the role line, which is all the record says.
    const row = {
      ...turn(15),
      messages: [message({ role: "user", chars: 0 })],
    };
    assertEquals(
      visibleMessages(row).map((slot) => slot.index),
      [0],
    );
    assertEquals(turnShape(row), "user");
  },
);

Deno.test("a text of blanks is no text, whatever the record counted", () => {
  // The record counts characters as stored; a text of spaces draws nothing,
  // so the pane has no block to open on it.
  const blanks = {
    ...turn(16),
    messages: [message({ chars: 900, text: " ".repeat(200) })],
  };
  assertEquals(
    visibleMessages(blanks).map((slot) => slot.index),
    [],
  );
  const ink = {
    ...turn(17),
    messages: [message({ chars: 900, text: `${" ".repeat(199)}x` })],
  };
  assertEquals(
    visibleMessages(ink).map((slot) => slot.index),
    [0],
  );
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

Deno.test("faceSupported reads one version, the one the CLI reads", () => {
  assertEquals(faceSupported(HISTORY_SCHEMA_VERSION), true);
  // An older face is refused too: the CLI refuses anything but its own
  // version, and two readers of one face must not disagree about it.
  assertEquals(faceSupported(HISTORY_SCHEMA_VERSION - 1), false);
  assertEquals(faceSupported(HISTORY_SCHEMA_VERSION + 1), false);
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
  assertExists(head);
  assertEquals(head.from, 100);
  assertEquals(head.atEnd, false);
  const tail = appendRange(
    head,
    body({ offset: 102, text: "cd", total_bytes: 200 }),
  );
  assertExists(tail);
  assertEquals(tail.from, 100);
  assertEquals(tail.to, 104);
  assertEquals(tail.text, "abcd");
  assertEquals(tail.more, null);
  assertEquals(tail.atEnd, true);
  const first = appendRange(emptyBodyText(), body());
  assertExists(first);
  assertEquals(first.text, "abc");
});

Deno.test(
  "appendRange calls a block empty only when every range was empty",
  () => {
    const blank = appendRange(null, body({ text: "", total_bytes: 0 }));
    assertExists(blank);
    assertEquals(blank.empty, true);
    const still = appendRange(blank, body({ text: "", total_bytes: 0 }));
    assertExists(still);
    assertEquals(still.empty, true);
    const filled = appendRange(
      still,
      body({ offset: 0, text: "x", total_bytes: 1 }),
    );
    assertExists(filled);
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
    assertExists(after);
    assertEquals(after.empty, false);
  },
);

Deno.test("appendRange refuses a range that does not continue the text", () => {
  const head = appendRange(
    null,
    body({ offset: 0, text: "AAA", truncated: true, next_offset: 3 }),
  );
  assertExists(head);
  // A gap and an overlap are both refusals: gluing either would produce a
  // string that exists nowhere in the log.
  assertEquals(appendRange(head, body({ offset: 6, text: "BBB" })), null);
  assertEquals(appendRange(head, body({ offset: 1, text: "BBB" })), null);
  const joined = appendRange(head, body({ offset: 3, text: "BBB" }));
  assertExists(joined);
  assertEquals(joined.text, "AAABBB");
});

Deno.test(
  "argsView reads the bit the face states, and calls only a prefix a prefix",
  () => {
    const whole = argsView(call({ args: '{"command":"ls -l"}' }), "detail");
    assertEquals(whole, {
      text: '{"command":"ls -l"}',
      more: false,
      complete: true,
    });
    // A marker inside the text is the model's own, not a prefix: only the
    // face's own bit makes one.
    assertEquals(
      argsView(call({ args: "echo a … b" }), "detail").complete,
      true,
    );
    const cut = argsView(
      call({ args: "a".repeat(200), args_more: true }),
      "detail",
    );
    assertEquals(cut.more, true);
    assertEquals(cut.text, "a".repeat(200));
    assertEquals(cut.complete, false);
    // A page's row is a summary whatever its length, so it is never complete.
    const page = argsView(call({ args: '{"command":"ls -l"}' }), "page");
    assertEquals(page.complete, false);
    assertEquals(page.more, false);
  },
);

Deno.test("a call body and a message body never share a slot", () => {
  assertEquals(bodyKey(3, 0), "3:0:result");
  assertEquals(bodyKey(3, 1, "args"), "3:1:args");
  assertEquals(bodyKey(3, 0, "text"), "3:0:text");
  assertEquals(bodyKey(3, 1, "reasoning"), "3:1:reasoning");
});

Deno.test(
  "startRead stamps the entry with the token it was started under",
  () => {
    const first = startRead(null, 7);
    assertEquals(first.token, 7);
    assertEquals(first.loading, true);
    assertEquals(first.text, "");
    // A read that starts over an existing block keeps what is on screen and
    // stamps itself, so its own answer can recognise the block it filled.
    const filled = finishRead(first, {
      kind: "text",
      text: { ...emptyBodyText(), text: "abc", atEnd: true },
    });
    const second = startRead(filled, 8);
    assertEquals(second.token, 8);
    assertEquals(second.text, "abc");
    assertEquals(second.loading, true);
  },
);

Deno.test("bodyStillMine compares the stamp, not the read's age", () => {
  const entry = startRead(null, 3);
  assertEquals(bodyStillMine(entry, 3), true);
  assertEquals(bodyStillMine(entry, 4), false);
  assertEquals(bodyStillMine(undefined, 3), false);
  // An answer keeps the stamp, so the block still belongs to the read that
  // filled it after the answer lands.
  const done = finishRead(entry, { kind: "failed", error: "boom" });
  assertEquals(done.token, 3);
  assertEquals(bodyStillMine(done, 3), true);
});

Deno.test("finishRead writes each outcome over the ranges already read", () => {
  const base = { ...startRead(null, 1), text: "kept", total: 4 };
  const unsupported = finishRead(base, {
    kind: "unsupported",
    face: HISTORY_SCHEMA_VERSION - 1,
  });
  assertEquals(unsupported.unsupported, HISTORY_SCHEMA_VERSION - 1);
  assertEquals(unsupported.error, null);
  assertEquals(unsupported.loading, false);
  assertEquals(unsupported.text, "kept");
  const failed = finishRead(base, { kind: "failed", error: "nope" });
  assertEquals(failed.error, "nope");
  assertEquals(failed.unsupported, null);
  const text = finishRead(base, {
    kind: "text",
    text: { ...emptyBodyText(), text: "next", total: 9, atEnd: true },
  });
  assertEquals(text.text, "next");
  assertEquals(text.total, 9);
  assertEquals(text.loading, false);
  assertEquals(text.error, null);
});

Deno.test("a block's key spaces are named apart", () => {
  assertEquals(bodyKey(7, 2), "7:2:result");
  assertEquals(bodyKey(7, 2, "args"), "7:2:args");
  assertEquals(bodyKey(7, 2, "text"), "7:2:text");
  assertEquals(reasoningKey(7, 2), "7:2:open");
});

// A turn opens with its calls' results already read, so the reader does not
// press a button per call to find out what ran. The cap is the page's stake
// against a turn of forty calls becoming forty requests at once.
Deno.test("a turn opens the results nobody has asked for, up to a cap", () => {
  const calls = Array.from({ length: 12 }, (_, index) =>
    call({ call_index: index }),
  );
  assertEquals(
    untouchedResults({ ...turn(7), calls }, {}),
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
  );
  assertEquals(AUTO_READ_CALLS, 10);
  assertEquals(untouchedResults({ ...turn(7), calls }, {}, 3), [0, 1, 2]);
});

Deno.test("a result on screen or in flight is not asked for again", () => {
  const calls = [call(), call({ call_index: 1 }), call({ call_index: 2 })];
  const held: Record<string, BodyEntry> = {
    [bodyKey(9, 1, "result")]: startRead(null, 4),
    [bodyKey(9, 2, "result")]: finishRead(startRead(null, 5), {
      kind: "failed",
      error: "boom",
    }),
  };
  // The failed one keeps its error and its retry button; reading it again
  // by itself would throw that answer away.
  assertEquals(untouchedResults({ ...turn(9), calls }, held), [0]);
});

Deno.test("another slot or another turn never stands in for a result", () => {
  const args: Record<string, BodyEntry> = {
    [bodyKey(3, 0, "args")]: startRead(null, 1),
  };
  assertEquals(untouchedResults({ ...turn(3), calls: [call()] }, args), [0]);
  const otherTurn: Record<string, BodyEntry> = {
    [bodyKey(4, 0, "result")]: startRead(null, 2),
  };
  assertEquals(
    untouchedResults({ ...turn(5), calls: [call()] }, otherTurn),
    [0],
  );
});

// A dot's ink is a table here, not a branch inside a component, and every
// paint carries both halves of a light/dark pair: one shade cannot clear 3:1
// on a light card and on a dark one. What this cannot see is a ramp the theme
// does not carry -- that class renders nothing at all -- so the pages get
// looked at in both modes.
Deno.test("toneDot paints each tone with a paired shade", () => {
  const painted = (["ok", "error", "info", "muted"] as const).map(toneDot);
  assertEquals(painted, [
    "bg-success-800-200",
    "bg-error-700-300",
    "bg-surface-700-300",
    "border border-surface-700-300",
  ]);
  for (const paint of painted) {
    for (const cls of paint.split(" ")) {
      if (cls === "border") continue;
      assertEquals(/^(bg|border)-[a-z]+-[0-9]+-[0-9]+$/.test(cls), true, cls);
    }
  }
});

// What the layout may do about the turn on show, and the symptom each rule
// keeps away: an empty pane beside a list that has turns, a stacked page
// pulled into a detail the reader did not ask for, a pick of the reader's
// replaced by a newer turn, and a pick of the layout's outliving the second
// column and hiding the list for good.
Deno.test(
  "two columns open the newest turn, and a stacked page opens none",
  () => {
    assertEquals(
      showOnRows({ panes: true, shown: NO_TURN, rows: [turn(3), turn(2)] }),
      { turnId: 3, auto: true },
    );
    assertEquals(
      showOnRows({ panes: false, shown: NO_TURN, rows: [turn(3)] }),
      NO_TURN,
    );
    assertEquals(
      showOnRows({ panes: true, shown: NO_TURN, rows: [] }),
      NO_TURN,
    );
  },
);

Deno.test("a turn the reader chose is never replaced by a newer one", () => {
  const chosen: ShownTurn = { turnId: 2, auto: false };
  assertEquals(
    showOnRows({ panes: true, shown: chosen, rows: [turn(3), turn(2)] }),
    chosen,
  );
});

Deno.test(
  "a change of width drops the layout's pick and keeps the reader's",
  () => {
    const auto: ShownTurn = { turnId: 3, auto: true };
    assertEquals(
      showOnWidth({ panes: false, shown: auto, rows: [turn(3)] }),
      NO_TURN,
    );
    const chosen: ShownTurn = { turnId: 3, auto: false };
    assertEquals(
      showOnWidth({ panes: false, shown: chosen, rows: [turn(3)] }),
      chosen,
    );
    assertEquals(
      showOnWidth({ panes: true, shown: NO_TURN, rows: [turn(4)] }),
      { turnId: 4, auto: true },
    );
  },
);
