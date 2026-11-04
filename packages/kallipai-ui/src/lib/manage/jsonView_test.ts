// Unit tests for the generic-view decision layer: which texts become trees,
// which numbers cannot be shown at all, what a folded node previews, which
// shape a call is drawn in, and how a terminal block's streams are read.

import { assert, assertEquals } from "@std/assert";
import {
  BLOCK_FOLD_LINES,
  buildJsonTree,
  callShape,
  childPath,
  foldsAt,
  foldString,
  isContainer,
  isJsonCandidate,
  JSON_MAX_DEPTH,
  JSON_MAX_NODES,
  lineCount,
  nodeSize,
  payloadObject,
  pickStream,
  envelopeError,
  envelopeFailed,
  nodeValue,
  plainScalar,
  previewOf,
  scalarText,
  STRING_FOLD_CHARS,
  STRING_FOLD_LINES,
  terminalOutput,
  terminalCommand,
  type JsonNode,
} from "./jsonView.ts";

function tree(text: string): JsonNode {
  const built = buildJsonTree(text);
  assert(built.ok, `expected a tree for ${text}`);
  return built.node;
}

// --- isJsonCandidate ------------------------------------------------------

Deno.test("candidate: an object or an array, nothing else", () => {
  assert(isJsonCandidate('{"a":1}'));
  assert(isJsonCandidate("  \n[1,2]"));
  assert(!isJsonCandidate("ok"));
  assert(!isJsonCandidate("42"));
  assert(!isJsonCandidate('"quoted"'));
  assert(!isJsonCandidate(""));
});

// --- buildJsonTree --------------------------------------------------------

Deno.test("tree: an object keeps key order and node kinds", () => {
  const node = tree('{"b":1,"a":"x","c":true,"d":null}');
  assertEquals(node.kind, "object");
  if (node.kind !== "object") return;
  assertEquals(
    node.entries.map((e) => e.key),
    ["b", "a", "c", "d"],
  );
  assertEquals(node.entries[0].node, { kind: "number", value: 1, text: "1" });
  assertEquals(node.entries[1].node, { kind: "string", value: "x" });
  assertEquals(node.entries[2].node, { kind: "boolean", value: true });
  assertEquals(node.entries[3].node, { kind: "null" });
});

Deno.test("tree: an array of objects nests", () => {
  const node = tree('[{"a":1}]');
  assertEquals(node.kind, "array");
  if (node.kind !== "array") return;
  assertEquals(node.items.length, 1);
  assertEquals(node.items[0].kind, "object");
});

Deno.test("tree: escapes and unicode arrive decoded", () => {
  const node = tree('{"s":"line\\nbreak \\u00e9"}');
  if (node.kind !== "object") throw new Error("expected an object");
  assertEquals(node.entries[0].node, {
    kind: "string",
    value: "line\nbreak é",
  });
});

Deno.test("tree: empty, prose and broken text name their reason", () => {
  assertEquals(buildJsonTree(""), { ok: false, reason: "empty" });
  assertEquals(buildJsonTree("   \n"), { ok: false, reason: "empty" });
  assertEquals(buildJsonTree("command not found"), {
    ok: false,
    reason: "not-json",
  });
  assertEquals(buildJsonTree('{"a":}'), { ok: false, reason: "invalid" });
  assertEquals(buildJsonTree("[1,2"), { ok: false, reason: "invalid" });
});

Deno.test(
  "tree: a number that would print wrong falls back, never rounds",
  () => {
    assertEquals(buildJsonTree('{"n":9007199254740993}'), {
      ok: false,
      reason: "unsafe-number",
    });
    assertEquals(buildJsonTree('{"n":1e400}'), {
      ok: false,
      reason: "unsafe-number",
    });
    // The last integer exactly representable: it must still render, digits
    // and all, so the guard is not a blanket refusal of large numbers.
    const node = tree('{"n":9007199254740991}');
    if (node.kind !== "object") throw new Error("expected an object");
    assertEquals(node.entries[0].node, {
      kind: "number",
      value: 9007199254740991,
      text: "9007199254740991",
    });
  },
);

Deno.test("tree: more nodes than the limit is refused", () => {
  const wide = `[${"1,".repeat(JSON_MAX_NODES)}1]`;
  assertEquals(buildJsonTree(wide), { ok: false, reason: "too-large" });
});

Deno.test("tree: deeper than the limit is refused", () => {
  const depth = JSON_MAX_DEPTH + 2;
  const deep = `${"[".repeat(depth)}1${"]".repeat(depth)}`;
  assertEquals(buildJsonTree(deep), { ok: false, reason: "too-large" });
});

Deno.test("tree: a number shows the digits it carried", () => {
  const node = tree('{"i":12,"f":1.5,"e":1e3}');
  if (node.kind !== "object") throw new Error("expected an object");
  assertEquals(
    node.entries.map((e) => scalarText(e.node)),
    ["12", "1.5", "1000"],
  );
});

// --- node helpers ---------------------------------------------------------

Deno.test("helpers: container, size and both scalar forms", () => {
  const node = tree('{"a":[1,2],"b":null}');
  assert(isContainer(node));
  assertEquals(nodeSize(node), 2);
  assertEquals(scalarText(node), null);
  assertEquals(nodeSize(tree("[1,2,3]")), 3);
  assertEquals(scalarText({ kind: "boolean", value: false }), "false");
  assertEquals(plainScalar({ kind: "string", value: "cd /tmp" }), "cd /tmp");
  assertEquals(plainScalar(node), null);
});

// --- previewOf ------------------------------------------------------------

Deno.test("preview: three named scalars, then an ellipsis", () => {
  const node = tree('{"command":"cd /tmp","timeout":120,"ok":true,"x":1}');
  assertEquals(
    previewOf(node),
    '{command: "cd /tmp", timeout: 120, ok: true, …}',
  );
});

Deno.test("preview: exactly three children carry no ellipsis", () => {
  assertEquals(previewOf(tree("[1,2,3]")), "[1, 2, 3]");
});

Deno.test("preview: containers only leave the line empty", () => {
  assertEquals(previewOf(tree('{"a":{"b":1},"c":[1]}')), "");
});

Deno.test("preview: long text is cut at the width asked for", () => {
  const node = tree(`{"k":"${"x".repeat(200)}"}`);
  const line = previewOf(node, 20);
  assertEquals(line.length, 20);
  assert(line.endsWith("…"));
});

// --- callShape ------------------------------------------------------------

const noFacts = { exitCode: null, stdoutBytes: null, stderrBytes: null };

Deno.test("shape: an exit status or a stream size means terminal", () => {
  assertEquals(callShape({ ...noFacts, exitCode: 0 }, ""), "terminal");
  assertEquals(callShape({ ...noFacts, stdoutBytes: 12 }, ""), "terminal");
  assertEquals(callShape({ ...noFacts, stderrBytes: 3 }, ""), "terminal");
});

Deno.test("shape: terminal outranks output that looks like a patch", () => {
  const patch = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n";
  assertEquals(callShape({ ...noFacts, exitCode: 1 }, patch), "terminal");
});

Deno.test("shape: diff, tree and text told apart by data", () => {
  assertEquals(callShape(noFacts, "@@ -1,2 +1,3 @@\n a\n+b\n"), "diff");
  assertEquals(callShape(noFacts, "diff --git a/x b/x\n"), "diff");
  assertEquals(callShape(noFacts, '{"ok":true}'), "json");
  assertEquals(callShape(noFacts, "no output"), "text");
  assertEquals(callShape(noFacts, ""), "text");
});

// --- terminalOutput -------------------------------------------------------

Deno.test("terminal: a merged-output envelope fills `merged`", () => {
  const node = tree(
    '{"ok":true,"tool_name":"bash_exec","result":{"output":"hi\\n",' +
      '"exit_code":0,"timed_out":false,"truncated":false}}',
  );
  assertEquals(terminalOutput(node), {
    stdout: null,
    stderr: null,
    merged: "hi\n",
    exitCode: 0,
    timedOut: false,
  });
});

Deno.test("terminal: a split envelope keeps stdout and stderr apart", () => {
  const node = tree('{"result":{"stdout":"out","stderr":"err","exit_code":2}}');
  const out = terminalOutput(node);
  assertEquals(out.stdout, "out");
  assertEquals(out.stderr, "err");
  assertEquals(out.exitCode, 2);
});

Deno.test("terminal: an unknown field type yields null, not a guess", () => {
  // A string where a number belongs is a record shape this build does not
  // know; showing "0" would invent a fact about how the program exited.
  assertEquals(terminalOutput(tree('{"output":"hi","exit_code":"0"}')), {
    stdout: null,
    stderr: null,
    merged: "hi",
    exitCode: null,
    timedOut: null,
  });
  assertEquals(terminalOutput(tree("[1,2]")), {
    stdout: null,
    stderr: null,
    merged: null,
    exitCode: null,
    timedOut: null,
  });
});

Deno.test("payloadObject: the result field wins over the envelope", () => {
  const payload = payloadObject(tree('{"ok":true,"result":{"output":"x"}}'));
  if (payload === null || payload.kind !== "object") {
    throw new Error("expected the result object");
  }
  assertEquals(
    payload.entries.map((e) => e.key),
    ["output"],
  );
  assertEquals(payloadObject(tree("[1]")), null);
});

// --- lines and folding ----------------------------------------------------

Deno.test("lines: a trailing newline adds no line", () => {
  assertEquals(lineCount(""), 0);
  assertEquals(lineCount("a"), 1);
  assertEquals(lineCount("a\n"), 1);
  assertEquals(lineCount("a\nb\n"), 2);
});

Deno.test("fold: the line and character thresholds", () => {
  const six = `${"l\n".repeat(STRING_FOLD_LINES - 1)}l`;
  assert(!foldsAt(six));
  assert(foldsAt(`${six}\nl`));
  assert(!foldsAt("x".repeat(STRING_FOLD_CHARS)));
  assert(foldsAt("x".repeat(STRING_FOLD_CHARS + 1)));
  assertEquals(BLOCK_FOLD_LINES, 12);
});

Deno.test("fold: a folded string keeps its first lines, then says so", () => {
  assertEquals(foldString("one"), "one…");
  const many = Array.from({ length: 9 }, (_, i) => `l${i}`).join("\n");
  assertEquals(foldString(many).split("\n").length, STRING_FOLD_LINES);
  const long = "x".repeat(STRING_FOLD_CHARS + 50);
  assertEquals(foldString(long).length, STRING_FOLD_CHARS + 1);
  assert(foldString(long).endsWith("…"));
});

Deno.test(
  "terminalCommand reads the command from a parsed argument, or nothing",
  () => {
    const args = buildJsonTree('{"command":"deno task test","timeout":900}');
    assert(args.ok);
    assertEquals(terminalCommand(args.ok ? args.node : null), "deno task test");
    // A multi-line command is one string, not a layout problem for this layer.
    const multi = buildJsonTree('{"command":"a\\nb"}');
    assertEquals(terminalCommand(multi.ok ? multi.node : null), "a\nb");
    const plain = buildJsonTree("printf hi");
    assertEquals(terminalCommand(plain.ok ? plain.node : null), null);
    const wrong = buildJsonTree('{"command":42}');
    assertEquals(terminalCommand(wrong.ok ? wrong.node : null), null);
    assertEquals(terminalCommand(null), null);
  },
);

Deno.test('childPath keeps keys holding "." or "[" apart', () => {
  const dotted = childPath("$text", "a.b");
  const nested = childPath(childPath("$text", "a"), "b");
  assert(dotted !== nested, "two paths must not share one fold key");
  const bracketed = childPath("$text", "a[0]");
  const indexed = childPath(childPath("$text", "a"), 0);
  assert(bracketed !== indexed);
  // The same step twice is the same key: folding state must follow the node.
  assertEquals(childPath("$text", "k"), childPath("$text", "k"));
  assertEquals(childPath("$text", 0) === childPath("$text", "0"), false);
});

// --- the payload readers a dedicated view shares -------------------------

Deno.test(
  "pickStream: the merged capture wins, else whichever stream is there",
  () => {
    const facts = (
      merged: string | null,
      stdout: string | null,
      stderr: string | null,
    ) => ({
      stdout,
      stderr,
      merged,
      exitCode: 0,
      timedOut: null,
    });
    assertEquals(pickStream(facts("both", "out", "err")), "both");
    assertEquals(pickStream(facts(null, "out", "err")), "out");
    assertEquals(pickStream(facts(null, null, "err")), "err");
    assertEquals(pickStream(facts(null, null, null)), null);
  },
);

Deno.test(
  "envelopeError: a failed envelope yields its first line, and never an empty one",
  () => {
    assertEquals(
      envelopeError(tree('{"ok":false,"error":"boom\\nsecond line"}')),
      "boom",
    );
    assertEquals(envelopeError(tree('{"ok":true,"result":{}}')), null);
    assertEquals(
      envelopeError(tree('{"ok":false}')),
      null,
      "a failure with no message is not reported as the words ok: false",
    );
    assertEquals(
      envelopeError(tree('{"ok":"false","error":"x"}')),
      null,
      "an ok that is not a boolean does not make the envelope failed",
    );
  },
);

Deno.test(
  "envelopeFailed: an ok that is false, whatever the message looks like",
  () => {
    assert(envelopeFailed(tree('{"ok":false}')));
    assert(
      envelopeFailed(tree('{"ok":false,"error":""}')),
      "an empty message still says the envelope failed",
    );
    assert(envelopeFailed(tree('{"ok":false,"error":"   \\n  "}')));
    assert(
      envelopeFailed(tree('{"ok":false,"error":42}')),
      "an error that is not a string does not make the envelope a success",
    );
    assert(!envelopeFailed(tree('{"ok":true,"result":{}}')));
    assert(
      !envelopeFailed(tree('{"ok":"false"}')),
      "an ok that is not a boolean is not a verdict",
    );
    assert(!envelopeFailed(tree('{"result":{}}')));
  },
);

Deno.test(
  "nodeValue: a __proto__ key stays a key, and the object has no prototype",
  () => {
    const value = nodeValue(tree('{"__proto__":{"polluted":true},"keep":1}'));
    if (typeof value !== "object" || value === null)
      throw new Error("expected an object");
    const record = value as Record<string, unknown>;
    assertEquals(Object.getPrototypeOf(record), null);
    assertEquals(Object.keys(record).sort(), ["__proto__", "keep"]);
    assertEquals(({} as Record<string, unknown>).polluted, undefined);
  },
);
