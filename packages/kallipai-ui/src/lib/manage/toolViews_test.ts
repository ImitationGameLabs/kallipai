// The registry's two legs: a validator that lets a real payload through (and
// ignores a field this build has never seen), and one that refuses a payload a
// view would read the wrong way. The picking is a pure function over a table,
// so every destination is one row here.
import { assert, assertEquals } from "@std/assert";
import type { Component } from "svelte";
import { buildJsonTree, type JsonNode, type JsonTree } from "./jsonView.ts";
import {
  bashExecFacts,
  bgReadFacts,
  bgReadTone,
  breakFacts,
  contextOpFacts,
  evictFacts,
  evictTone,
  isBashExecResult,
  formatBytes,
  isBgReadResult,
  isBreakResult,
  isContextPinResult,
  isContextUnpinResult,
  isEvictResult,
  parseEnvelope,
  pickView,
  readEnvelope,
  shortPath,
  shortId,
  type BgReadResult,
  view,
  type CallViewProps,
  type ContextPinResult,
  type ContextUnpinResult,
  type ParsedEnvelope,
  type ToolView,
} from "./toolViews.ts";

/** The table's component slot, unused here: these tests read decisions, not
 * markup (the source pins cover the template wiring). */
const placeholder = null as unknown as Component<CallViewProps<unknown>>;

/** The tree a passing payload came from; these tests read the value. */
const NODE: JsonNode = { kind: "null" };

/** A text that is not here yet: the fourth answer the picker takes. */
const UNREADABLE: ParsedEnvelope = { kind: "unreadable" };

/** The tree a stored text builds, so a shape can be written down as the
 * record holds it. */
function tree(text: string): JsonNode {
  const built = buildJsonTree(text);
  assert(built.ok, `expected a tree for ${text}`);
  return built.node;
}

const VIEWS: Record<string, ToolView> = {
  bash_exec: view(isBashExecResult, placeholder),
  break: view(isBreakResult, placeholder),
  bash_background_read: view(isBgReadResult, placeholder),
  context_evict: view(isEvictResult, placeholder),
  context_pin: view(isContextPinResult, placeholder),
  context_unpin: view(isContextUnpinResult, placeholder),
};

const BASH: Record<string, unknown> = {
  exit_code: 0,
  timed_out: false,
  truncated: false,
  cwd: "/tmp",
  output: "hello\n",
  future_field: { a: 1 },
};

const BREAK: Record<string, unknown> = {
  parked: true,
  until: "wait",
  timeout_secs: 600,
  future_field: { a: 1 },
};

const BG: Record<string, unknown> = {
  task_id: "979e855d-aaaa",
  output: "line one\nline two\n",
  state: "exited",
  stalled: false,
  bytes: 66,
  exit_code: 0,
  future_field: { a: 1 },
};

const EVICT: Record<string, unknown> = {
  evicted: 34,
  remaining_turns: 20,
  freed_tokens: 203225,
  kept_turns: 20,
  first_kept_turn: 58779,
  max_turns: 20,
  max_tokens: 43771,
  note: null,
  degraded: null,
  future_field: { a: 1 },
};

const PIN: Record<string, unknown> = {
  pinned: "skill:index",
  pinned_labels: ["kallip:reference", "skill:index"],
  future_field: { a: 1 },
};

const UNPIN: Record<string, unknown> = {
  unpinned: "kallip:reference",
  pinned_labels: ["skill:index"],
  future_field: { a: 1 },
};

Deno.test(
  "a real payload with a field this build never saw is accepted",
  () => {
    assert(isBashExecResult(BASH), "bash_exec keeps its view when it grows");
    assert(isBreakResult(BREAK), "break keeps its view when it grows");
    assertEquals(
      pickView(
        "bash_exec",
        { kind: "payload", payload: BASH, node: NODE },
        VIEWS,
      ).kind,
      "dedicated",
    );
    assertEquals(
      pickView("break", { kind: "payload", payload: BREAK, node: NODE }, VIEWS)
        .kind,
      "dedicated",
    );
  },
);

Deno.test("a payload a view would misread is refused", () => {
  const missing = { timed_out: false, truncated: false, cwd: "/tmp" };
  assert(!isBashExecResult(missing), "exit_code is read, so it must be there");
  assert(
    !isBashExecResult({ ...BASH, exit_code: "0" }),
    "the code is a number",
  );
  assert(
    !isBashExecResult({ ...BASH, truncated: "no" }),
    "the flag is a boolean",
  );
  assert(!isBashExecResult({ ...BASH, task_id: 7 }), "an id is a string");
  assert(
    !isBreakResult({ until: "idle" }),
    "parked is read, so it must be there",
  );
  assert(!isBreakResult({ parked: 1 }), "parked is a boolean");
  for (const value of [null, [], "x", 3]) {
    assert(!isBashExecResult(value), "a non-object has no fields to read");
    assert(!isBreakResult(value), "a non-object has no fields to read");
  }
});

Deno.test(
  "a registered tool whose payload was refused falls back, and says so",
  () => {
    const pick = pickView(
      "bash_exec",
      { kind: "payload", payload: { exit_code: 0 }, node: NODE },
      VIEWS,
    );
    assertEquals(pick.kind, "generic");
    if (pick.kind === "generic") {
      assert(
        pick.registered,
        "the fallback line is only for a registered tool",
      );
    }
  },
);

Deno.test("the picking covers every destination", () => {
  const unknownTool = pickView("bash_future", UNREADABLE, VIEWS);
  assertEquals(unknownTool.kind, "generic");
  if (unknownTool.kind === "generic") {
    assert(
      !unknownTool.registered,
      "an unregistered name draws no fallback line",
    );
  }

  const unread = pickView("bash_exec", UNREADABLE, VIEWS);
  assertEquals(unread.kind, "generic");
  if (unread.kind === "generic") {
    assert(!unread.registered, "a text not read to the end is not a fallback");
  }

  const failed = pickView(
    "bash_exec",
    { kind: "failed", error: "boom" },
    VIEWS,
  );
  assertEquals(failed.kind, "failed");
  if (failed.kind === "failed") assertEquals(failed.error, "boom");

  const inherited = pickView(
    "constructor",
    { kind: "payload", payload: BASH, node: NODE },
    VIEWS,
  );
  assertEquals(
    inherited.kind,
    "generic",
    "a name from the prototype is no view",
  );
  if (inherited.kind === "generic") {
    assert(!inherited.registered, "and it does not claim a registration");
  }
});

Deno.test("a failed envelope is a result, not a fallback", () => {
  const parsed: ParsedEnvelope = { kind: "failed", error: "tool not run" };
  const pick = pickView("break", parsed, VIEWS);
  assertEquals(
    pick.kind,
    "failed",
    "the dedicated row says the tool did not run",
  );
});

Deno.test("parseEnvelope: a failed envelope never reaches a validator", () => {
  for (const text of [
    '{"ok":false}',
    '{"ok":false,"error":""}',
    '{"ok":false,"error":"   \\n  "}',
    '{"ok":false,"error":42}',
  ]) {
    assertEquals(
      parseEnvelope(tree(text)),
      { kind: "failed", error: null },
      `${text} is a failure with nothing to say`,
    );
  }
  assertEquals(parseEnvelope(tree('{"ok":false,"error":"boom\\nsecond"}')), {
    kind: "failed",
    error: "boom",
  });
});

Deno.test(
  "the worst cell: a failure carrying a valid payload is a failure",
  () => {
    const parsed = parseEnvelope(
      tree(JSON.stringify({ ok: false, result: BASH })),
    );
    assertEquals(parsed, { kind: "failed", error: null });
    const pick = pickView("bash_exec", parsed, VIEWS);
    assertEquals(
      pick.kind,
      "failed",
      "a failure is never drawn as the tool's own view",
    );
  },
);

Deno.test("parseEnvelope: a success reads down to its payload", () => {
  const parsed = parseEnvelope(
    tree(JSON.stringify({ ok: true, result: BASH })),
  );
  assertEquals(parsed.kind, "payload");
  if (parsed.kind === "payload") {
    assertEquals(parsed.payload, BASH);
    assertEquals(parsed.node.kind, "object");
  }
  const bare = parseEnvelope(tree('{"exit_code":0}'));
  assertEquals(
    bare.kind,
    "payload",
    "a text that is its own payload parses as one",
  );
  const direct = parseEnvelope(tree('{"result":{"exit_code":0}}'));
  assertEquals(direct.kind, "payload");
  for (const text of ['{"ok":"false"}', '{"ok":1}', '{"result":{}}']) {
    assertEquals(
      parseEnvelope(tree(text)).kind,
      "payload",
      `${text} is not a failure`,
    );
  }
});

Deno.test(
  "parseEnvelope: a deferred call carries an approval, not a payload",
  () => {
    const full = parseEnvelope(
      tree(
        JSON.stringify({
          ok: true,
          tool_name: "bash_exec",
          pending_approval: true,
          id: "ap_1",
          next_steps: "Call approval_commit with the id.",
        }),
      ),
    );
    assertEquals(full, {
      kind: "pending",
      pending: { id: "ap_1" },
    });
    assertEquals(
      parseEnvelope(tree('{"ok":true,"pending_approval":true}')),
      { kind: "pending", pending: { id: null } },
      "the fallback envelope names no approval",
    );
    assertEquals(
      parseEnvelope(
        tree('{"ok":true,"pending_approval":false,"result":{"exit_code":0}}'),
      ).kind,
      "payload",
      "a false flag is a success envelope, not a deferral",
    );
    assertEquals(
      parseEnvelope(tree('{"ok":false,"pending_approval":true,"error":"no"}')),
      { kind: "failed", error: "no" },
      "a failure that also names an approval is still a failure",
    );
    assertEquals(
      parseEnvelope(tree('{"id":"ap_1"}')).kind,
      "payload",
      "an id alone defers nothing",
    );
  },
);

Deno.test("a deferral and a failure speak for any tool name", () => {
  const pending = parseEnvelope(
    tree('{"ok":true,"pending_approval":true,"id":"ap_9"}'),
  );
  const pick = pickView("aifed", pending, VIEWS);
  assertEquals(pick.kind, "pending", "an unregistered tool still waits");
  if (pick.kind === "pending") assertEquals(pick.pending.id, "ap_9");

  const failed = parseEnvelope(tree('{"ok":false,"error":"denied"}'));
  const unregistered = pickView("aifed", failed, VIEWS);
  assertEquals(
    unregistered.kind,
    "failed",
    "an unregistered tool borrows the failure line",
  );
  if (unregistered.kind === "failed") {
    assertEquals(unregistered.error, "denied");
  }
});

Deno.test("shortPath: the last two segments, or the path as it stands", () => {
  assertEquals(shortPath("/tmp"), "/tmp");
  assertEquals(shortPath("/a/b"), "/a/b");
  assertEquals(shortPath("/a/b/c"), "…/b/c");
  assertEquals(
    shortPath("/home/dev/kallipai-workspace/dev/kallipai-dev/"),
    "…/dev/kallipai-dev",
    "a trailing slash is not a segment",
  );
});

Deno.test("facts project how a call ended, and nothing else", () => {
  const running = bashExecFacts({
    exit_code: null,
    timed_out: true,
    truncated: true,
    cwd: "/w",
    task_id: "bg-1",
  });
  assertEquals(running, {
    state: "running",
    exit: null,
    timedOut: true,
    truncated: true,
    cwd: "/w",
    taskId: "bg-1",
  });
  const ok = bashExecFacts({
    exit_code: 0,
    timed_out: false,
    truncated: false,
    cwd: "/",
  });
  assertEquals(ok.state, "succeeded");
  assertEquals(ok.taskId, null);
  assertEquals(
    bashExecFacts({
      exit_code: 2,
      timed_out: false,
      truncated: false,
      cwd: "/",
    }).state,
    "failed",
  );
  assertEquals(
    bashExecFacts({
      exit_code: null,
      timed_out: false,
      truncated: false,
      cwd: "/",
    }).state,
    "no-code",
    "a missing exit code with no task id has no background run to point at",
  );

  assertEquals(breakFacts({ parked: true }), {
    state: "paused-unrecorded",
    timer: null,
  });
  assertEquals(
    breakFacts({ parked: true, until: "typo" }),
    {
      state: "paused-unrecorded",
      timer: null,
    },
    "a rule this build cannot read is not a timer",
  );
  assertEquals(breakFacts({ parked: false }), {
    state: "not-paused",
    timer: null,
  });
  assertEquals(breakFacts({ parked: true, until: "idle" }), {
    state: "paused-message",
    timer: null,
  });
  assertEquals(
    breakFacts({ parked: true, until: "wait", timeout_secs: 1 }).timer,
    { unit: "seconds", value: 1 },
    "a one-second timer is not rounded up to a minute",
  );
  assertEquals(
    breakFacts({ parked: true, until: "wait", timeout_secs: 30 }).timer,
    { unit: "seconds", value: 30 },
  );
  assertEquals(
    breakFacts({ parked: true, until: "wait", timeout_secs: 89 }).timer,
    { unit: "minutes", value: 2 },
    "a minute and a half rounds up, so the line never promises an early wake",
  );
  assertEquals(
    breakFacts({ parked: true, until: "wait", timeout_secs: 90 }).timer,
    { unit: "minutes", value: 2 },
  );
  assertEquals(
    breakFacts({ parked: true, until: "wait", timeout_secs: 600 }).timer,
    { unit: "minutes", value: 10 },
  );
  assertEquals(breakFacts({ parked: true, until: "wait" }), {
    state: "paused-timer",
    timer: null,
  });
});

Deno.test(
  "a background read keeps its view, and a shape it would misread is refused",
  () => {
    assert(isBgReadResult(BG), "the real shape parses");
    assert(
      !isBgReadResult({ ...BG, state: 3 }),
      "the state word is a string, since a view prints it",
    );
    assert(!isBgReadResult({ ...BG, bytes: "12" }), "bytes is a number");
    assert(
      !isBgReadResult({ ...BG, stalled: null }),
      "stalled is read, so a null is not its absence",
    );
    const { task_id: _dropped, ...noId } = BG;
    assert(!isBgReadResult(noId), "the task id is how a view names the task");
    for (const value of [null, [], "x", 3]) {
      assert(!isBgReadResult(value), "a non-object has no fields to read");
    }
  },
);

Deno.test(
  "an eviction keeps its view, and a shape it would misread is refused",
  () => {
    assert(isEvictResult(EVICT), "the timer-path shape parses");
    const old = { evicted: 54, remaining_turns: 0, freed_tokens: 203225 };
    assert(
      isEvictResult(old),
      "a record without the window fields still parses",
    );
    assert(
      isEvictResult({ ...EVICT, first_kept_turn: null, degraded: null }),
      "an empty window and no degradation are stated as nulls",
    );
    assert(
      !isEvictResult({ ...EVICT, freed_tokens: "203225" }),
      "a count is a number",
    );
    assert(
      !isEvictResult({ ...EVICT, degraded: true }),
      "the degradation is a reason string",
    );
    assert(
      isEvictResult({ ...EVICT, note: "anything" }),
      "the note is the tool's wording, not a field this view reads",
    );
    for (const value of [null, [], "x"]) {
      assert(!isEvictResult(value), "a non-object has no fields to read");
    }
    const { evicted: _dropped, ...noCount } = EVICT;
    assert(!isEvictResult(noCount), "what it drained is read");
  },
);

Deno.test("a tool with a view draws its own body when its shape parses", () => {
  for (const [name, payload] of [
    ["bash_background_read", BG],
    ["context_evict", EVICT],
    ["context_pin", PIN],
    ["context_unpin", UNPIN],
  ] as const) {
    const pick = pickView(
      name,
      { kind: "payload", payload, node: NODE },
      VIEWS,
    );
    assertEquals(pick.kind, "dedicated", `${name} keeps its own view`);
  }
});

Deno.test(
  "background read facts: four states, an unknown word, and a tail that folds",
  () => {
    const running = bgReadFacts({
      task_id: "t",
      output: "",
      state: "running",
      stalled: true,
      bytes: 0,
    });
    assertEquals(running.state, "running");
    assertEquals(running.stalled, true);
    assertEquals(running.exit, null);
    assertEquals(running.tail, "");
    assertEquals(running.folded, false, "an empty tail has nothing to fold");

    const failed = bgReadFacts({
      task_id: "t",
      output: "x",
      state: "exited",
      stalled: false,
      bytes: 3,
      exit_code: 2,
    });
    assertEquals(failed.state, "exited");
    assertEquals(failed.exit, 2);
    assertEquals(
      bgReadFacts({
        task_id: "t",
        output: "",
        state: "killed",
        stalled: false,
        bytes: 0,
      }).state,
      "killed",
    );
    const unknown = bgReadFacts({
      task_id: "t",
      output: "",
      state: "paused",
      stalled: false,
      bytes: 0,
    });
    assertEquals(unknown.state, "other");
    assertEquals(
      unknown.stateWord,
      "paused",
      "the record's word is kept, not guessed",
    );
    assertEquals(
      bgReadFacts({
        task_id: "t",
        output: "",
        state: "exited",
        stalled: false,
        bytes: 0,
      }).state,
      "other",
      "an exit without a code is not an outcome this view prints",
    );
    const long = Array.from({ length: 13 }, (_, i) => `line ${i}`).join("\n");
    assertEquals(
      bgReadFacts({
        task_id: "t",
        output: long,
        state: "running",
        stalled: false,
        bytes: 900,
      }).folded,
      true,
      "thirteen lines fold at the block's own limit",
    );
    assertEquals(
      bgReadFacts({
        task_id: "t",
        output: "one\n",
        state: "running",
        stalled: false,
        bytes: 4,
      }).folded,
      false,
    );
  },
);

Deno.test(
  "eviction facts: the drained, the window, a cap of 0, and a degraded slice",
  () => {
    assertEquals(
      evictFacts({ evicted: 54, remaining_turns: 0, freed_tokens: 203225 }),
      {
        evicted: 54,
        total: 54,
        freed: 203225,
        kept: null,
        degraded: null,
        capZero: false,
      },
      "an old record has no window to report",
    );
    const window = evictFacts({
      evicted: 34,
      remaining_turns: 20,
      freed_tokens: 203225,
      kept_turns: 20,
      max_turns: 20,
      max_tokens: 43771,
    });
    assertEquals(window.kept, {
      keptTurns: 20,
      maxTurns: 20,
      maxTokens: 43771,
    });
    assertEquals(window.total, 54, "what it drained plus what the window left");
    assertEquals(window.capZero, false);
    const degraded = evictFacts({
      evicted: 3,
      remaining_turns: 0,
      freed_tokens: 9,
      kept_turns: 0,
      max_turns: 20,
      max_tokens: 100,
      degraded: "newest turn exceeds max_tokens",
    });
    assertEquals(degraded.kept?.keptTurns, 0, "the window kept nothing");
    assertEquals(degraded.degraded, "newest turn exceeds max_tokens");
    assertEquals(degraded.capZero, false);
    const capZero = evictFacts({
      evicted: 5,
      remaining_turns: 0,
      freed_tokens: 5,
      kept_turns: 0,
      max_turns: 0,
      max_tokens: 100,
    });
    assertEquals(capZero.capZero, true, "a turn cap of 0 condenses every turn");
  },
);

Deno.test(
  "a pin and an unpin keep their views, and a shape they would misread is refused",
  () => {
    assert(isContextPinResult(PIN), "the real shape parses");
    assert(isContextUnpinResult(UNPIN), "the real shape parses");
    assert(
      isContextPinResult({ pinned: "last", pinned_labels: [] }),
      "unpinning the last label leaves an empty list, which is still the shape",
    );
    assert(
      !isContextUnpinResult(PIN) && !isContextPinResult(UNPIN),
      "each verb names its own field, so one reply is not the other",
    );
    assert(
      !isContextPinResult({ ...PIN, pinned_labels: [1] }),
      "the labels are strings, since a view prints them",
    );
    assert(!isContextPinResult({ ...PIN, pinned: 3 }), "the label is a string");
    const { pinned: _dropped, ...noLabel } = PIN;
    assert(!isContextPinResult(noLabel), "the label this call named is read");
    assert(
      !isContextUnpinResult({ ...UNPIN, pinned_labels: "one" }),
      "a single label is not the list the record sends",
    );
    for (const value of [null, [], "x", 3]) {
      assert(!isContextPinResult(value), "a non-object has no fields to read");
      assert(
        !isContextUnpinResult(value),
        "a non-object has no fields to read",
      );
    }
  },
);
Deno.test(
  "pin facts: the name picks the verb, and too many labels means no chips",
  () => {
    const two = contextOpFacts("context_pin", {
      pinned: "skill:index",
      pinned_labels: ["kallip:reference", "skill:index"],
    });
    assertEquals(two, {
      verb: "pinned",
      label: "skill:index",
      labels: ["kallip:reference", "skill:index"],
      total: 2,
    });
    const eight = contextOpFacts("context_pin", {
      pinned: "p7",
      pinned_labels: Array.from({ length: 8 }, (_, i) => `p${i}`),
    });
    assertEquals(
      eight.labels.length,
      8,
      "eight labels are exactly what the chips show",
    );
    const many = contextOpFacts("context_pin", {
      pinned: "p37",
      pinned_labels: Array.from({ length: 38 }, (_, i) => `p${i}`),
    });
    assertEquals(many.labels, [], "a set too big to show whole draws no chips");
    assertEquals(many.total, 38);
    assertEquals(
      contextOpFacts("context_unpin", { unpinned: "p0", pinned_labels: [] }),
      { verb: "unpinned", label: "p0", labels: [], total: 0 },
      "unpinning the last label leaves nothing for the chips",
    );
    const stray = contextOpFacts("context_pin", {
      pinned: "mine",
      pinned_labels: ["mine"],
      unpinned: "UNCHECKED",
    } as unknown as ContextPinResult);
    assertEquals(
      stray.verb,
      "pinned",
      "the name decides the verb, not the payload",
    );
    assertEquals(
      stray.label,
      "mine",
      "the cast reads the verb's field, not whichever key is present",
    );
    const reverse = contextOpFacts("context_unpin", {
      pinned: "x",
      pinned_labels: [],
      unpinned: "p0",
    } as unknown as ContextUnpinResult);
    assertEquals(
      reverse.verb,
      "unpinned",
      "the name decides the verb, not the payload",
    );
    assertEquals(
      reverse.label,
      "p0",
      "the cast reads the verb's field, not whichever key is present",
    );
  },
);

Deno.test("a call's dot reports the state its sentence names", () => {
  const bg = (patch: Partial<BgReadResult>) =>
    bgReadTone(
      bgReadFacts({
        task_id: "t",
        output: "",
        state: "running",
        stalled: false,
        bytes: 0,
        ...patch,
      }),
    );
  assertEquals(bg({}), "info", "still running is not an alarm");
  assertEquals(
    bg({ stalled: true }),
    "info",
    "a stalled task keeps the running dot; the line under it carries the alarm",
  );
  assertEquals(bg({ state: "exited", exit_code: 0 }), "ok");
  assertEquals(bg({ state: "exited", exit_code: 2 }), "error");
  assertEquals(bg({ state: "killed" }), "error");
  assertEquals(
    bg({ state: "paused" }),
    "muted",
    "a word it cannot read is neutral",
  );
  const evict = (evicted: number) =>
    evictTone(evictFacts({ evicted, remaining_turns: 0, freed_tokens: 0 }));
  assertEquals(evict(0), "muted", "nothing condensed is not a success");
  assertEquals(evict(3), "ok");
});

Deno.test("a byte count is shown at a scale a reader can compare", () => {
  assertEquals(formatBytes(0), "0 B");
  assertEquals(formatBytes(1023), "1023 B");
  assertEquals(formatBytes(1024), "1 KB");
  assertEquals(formatBytes(1536), "1.5 KB");
  assertEquals(formatBytes(578579), "565 KB");
  assertEquals(formatBytes(3 * 1024 * 1024), "3 MB");
  assertEquals(formatBytes(1024 * 1024 - 1), "1 MB");
  assertEquals(formatBytes(1024 * 1024), "1 MB");
});

Deno.test("an id is cut once, in one place", () => {
  assertEquals(
    shortId("ap_242e8"),
    "ap_242e8",
    "eight characters fit as they are",
  );
  assertEquals(shortId("ap_242e8363d9b547f399d35520477ea85e"), "ap_242e8…");
});

Deno.test("a text that is not here is its own answer", () => {
  const missing: JsonTree = { ok: false, reason: "empty" };
  assertEquals(readEnvelope(missing), UNREADABLE);
  const built = buildJsonTree('{"ok":true,"x":1}');
  assert(built.ok);
  const read = readEnvelope(built);
  assertEquals(read.kind, "payload");
});
