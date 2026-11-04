// Pure layer behind the generic JSON view and the call-shape decision: it
// turns one stored string into a tree the page can draw without knowing any
// tool's schema, and it says which shape a call's text should be shown in.
// The page owns the reads and the wording; this module owns two questions --
// can this text be shown as a tree, and which shape is it.
//
// Three rules shape the code below:
//
// - A number that cannot be shown exactly is not shown at all. `JSON.parse`
//   maps an overflowing literal (`1e400`) to `Infinity` and rounds integers
//   past 2^53, and either would print digits the record does not carry. Such
//   a text falls back to the raw view (`unsafe-number`) -- the original is
//   authoritative, an approximation is a lie.
// - The limits are checked before the tree is built, walking an explicit
//   stack. A megabyte-sized argument must cost a bounded amount of work, and
//   the check must not depend on the engine's recursion budget.
// - Shape is judged by data, never by tool name. The same tool can answer
//   with a tree, a terminal block or plain prose, and a name-based guess
//   would be wrong exactly where it matters.

/** Nodes a tree may hold before the view gives up and shows the raw text. */
export const JSON_MAX_NODES = 2000;

/** Levels a tree may nest (root is 0) before the view gives up. */
export const JSON_MAX_DEPTH = 12;

/** Lines a string value shows before it folds behind `Show all`. */
export const STRING_FOLD_LINES = 6;

/** Characters a string value shows before it folds behind `Show all`. */
export const STRING_FOLD_CHARS = 400;

/** Lines above which a kept tail counts as folded: a background read keeps
 * only the latest lines, and its view says so. */
export const BLOCK_FOLD_LINES = 12;

/** One value of a parsed text. Object keys keep the order the text had. */
export type JsonNode =
  | { kind: "object"; entries: JsonEntry[] }
  | { kind: "array"; items: JsonNode[] }
  | { kind: "string"; value: string }
  | { kind: "number"; value: number; text: string }
  | { kind: "boolean"; value: boolean }
  | { kind: "null" };

export type JsonEntry = { key: string; node: JsonNode };

/** Why a text was not turned into a tree. `empty` and `not-json` are the
 * ordinary answers (a call that printed prose, a body not read yet); the
 * other three are cases the page explains instead of pretending. */
export type JsonFailure =
  | "empty"
  | "not-json"
  | "invalid"
  | "too-large"
  | "unsafe-number";

export type JsonTree =
  | { ok: true; node: JsonNode }
  | { ok: false; reason: JsonFailure };

/** Whether a text is worth feeding to the tree at all: a bare scalar
 * (`42`, `"ok"`, `true`) reads better as text than as a one-line tree. */
export function isJsonCandidate(text: string): boolean {
  const first = text.trimStart().charAt(0);
  return first === "{" || first === "[";
}

/**
 * Parse a stored text into a tree, or name why it cannot be one.
 *
 * `JSON.parse` is used on purpose: escapes and unicode are the engine's
 * business, and a hand-written parser would be a second source of truth
 * about what the record says. The price is numeric fidelity, paid by
 * `unsafe-number` below -- the text is measured first, so a number that
 * would print wrong is caught before any node exists.
 */
export function buildJsonTree(text: string): JsonTree {
  if (text.trim() === "") return { ok: false, reason: "empty" };
  if (!isJsonCandidate(text)) return { ok: false, reason: "not-json" };
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { ok: false, reason: "invalid" };
  }
  const failure = measure(parsed);
  if (failure !== null) return { ok: false, reason: failure };
  return { ok: true, node: toNode(parsed) };
}

/** The text is a tree only when every value it holds can be shown as it was
 * written; this pass answers that with one bounded walk. */
function measure(root: unknown): "too-large" | "unsafe-number" | null {
  let nodes = 0;
  const stack: { value: unknown; depth: number }[] = [
    { value: root, depth: 0 },
  ];
  while (stack.length > 0) {
    const frame = stack.pop() as { value: unknown; depth: number };
    nodes += 1;
    if (nodes > JSON_MAX_NODES) return "too-large";
    if (frame.depth > JSON_MAX_DEPTH) return "too-large";
    const value = frame.value;
    if (typeof value === "number") {
      // `NaN` cannot come out of `JSON.parse`; `Infinity` can, from a literal
      // that overflows the double range.
      if (!Number.isFinite(value)) return "unsafe-number";
      if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
        return "unsafe-number";
      }
      continue;
    }
    if (Array.isArray(value)) {
      for (const item of value) {
        stack.push({ value: item, depth: frame.depth + 1 });
      }
      continue;
    }
    if (value !== null && typeof value === "object") {
      for (const item of Object.values(value as Record<string, unknown>)) {
        stack.push({ value: item, depth: frame.depth + 1 });
      }
    }
  }
  return null;
}

/** Build the tree. Recursion is safe here because `measure` already capped
 * the depth at `JSON_MAX_DEPTH`. */
function toNode(value: unknown): JsonNode {
  if (value === null) return { kind: "null" };
  if (typeof value === "string") return { kind: "string", value };
  if (typeof value === "boolean") return { kind: "boolean", value };
  if (typeof value === "number") {
    return { kind: "number", value, text: numberText(value) };
  }
  if (Array.isArray(value)) {
    return { kind: "array", items: value.map(toNode) };
  }
  const record = value as Record<string, unknown>;
  return {
    kind: "object",
    entries: Object.entries(record).map(([key, node]) => ({
      key,
      node: toNode(node),
    })),
  };
}

/** The spelling a number is shown in. For every integer the view accepts
 * this is the digits on the wire; for a fraction or an exponent it is the
 * shortest spelling that parses back to the same double, so the value is
 * exact even where its written form is not reproduced. */
function numberText(value: number): string {
  return String(value);
}

/** Whether a node holds children. */
export function isContainer(node: JsonNode): boolean {
  return node.kind === "object" || node.kind === "array";
}

/** How many children a node holds (0 for scalars). */
export function nodeSize(node: JsonNode): number {
  if (node.kind === "object") return node.entries.length;
  if (node.kind === "array") return node.items.length;
  return 0;
}

/** A scalar's one-line form, JSON-style (strings keep their quotes, newlines
 * stay escaped). `null` for a container: a folded container is summarised by
 * its own counts, not by its children. */
export function scalarText(node: JsonNode): string | null {
  if (node.kind === "string") return JSON.stringify(node.value);
  if (node.kind === "number") return node.text;
  if (node.kind === "boolean") return node.value ? "true" : "false";
  if (node.kind === "null") return "null";
  return null;
}

/** A scalar's bare form, for a summary line (`command: cd /tmp && ls` reads
 * better without the quotes a JSON preview needs). */
export function plainScalar(node: JsonNode): string | null {
  if (node.kind === "string") return node.value;
  if (node.kind === "number") return node.text;
  if (node.kind === "boolean") return node.value ? "true" : "false";
  if (node.kind === "null") return "null";
  return null;
}

/** One line that says what a folded container holds: its first three scalar
 * children, JSON-style, cut at `max` characters. Empty when every child is a
 * container: the row then shows only its own counts, which is honest. */
export function previewOf(node: JsonNode, max = 80): string {
  const parts: string[] = [];
  const scalar = (child: JsonNode) => {
    if (parts.length === 3) return;
    const text = scalarText(child);
    if (text !== null) parts.push(text);
  };
  if (node.kind === "object") {
    for (const entry of node.entries) {
      if (parts.length === 3) break;
      const text = scalarText(entry.node);
      if (text !== null) parts.push(`${entry.key}: ${text}`);
    }
  } else if (node.kind === "array") {
    for (const item of node.items) scalar(item);
  }
  if (parts.length === 0) return "";
  const more = nodeSize(node) > parts.length ? ", …" : "";
  return clip(
    node.kind === "array"
      ? `[${parts.join(", ")}${more}]`
      : `{${parts.join(", ")}${more}}`,
    max,
  );
}

/** Cut text to `max` characters, ending in an ellipsis. A surrogate pair is
 * never split. */
function clip(text: string, max: number): string {
  if (text.length <= max) return text;
  let cut = Math.max(0, max - 1);
  const code = text.charCodeAt(cut - 1);
  if (code >= 0xd800 && code <= 0xdbff) cut -= 1;
  return `${text.slice(0, cut)}…`;
}

/** The facts a call row carries about its streams, taken from the wire
 * (`CallRow.exit_code` / `stdout_bytes` / `stderr_bytes`). */
export type StreamFacts = {
  exitCode: number | null;
  stdoutBytes: number | null;
  stderrBytes: number | null;
};

/** Shapes a call's text can be shown in, in the order they are tried. */
export type CallShape = "terminal" | "diff" | "json" | "text";

const DIFF_HUNK = /^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@/m;
const DIFF_GIT = /^diff --git /m;

/**
 * Which shape to draw for one call.
 *
 * A call that reports an exit status or a stream size ran something, so it
 * is drawn as a terminal even when its output happens to parse as JSON --
 * the program's output is the fact, the coincidence is not. Diff comes next
 * because a patch is only readable line by line; then a tree; then prose.
 */
export function callShape(facts: StreamFacts, text: string): CallShape {
  if (
    facts.exitCode !== null ||
    facts.stdoutBytes !== null ||
    facts.stderrBytes !== null
  ) {
    return "terminal";
  }
  if (DIFF_HUNK.test(text) || DIFF_GIT.test(text)) return "diff";
  if (isJsonCandidate(text)) return "json";
  return "text";
}

/** What a terminal-shaped call carries besides its facts. `stdout` and
 * `stderr` exist only when the record kept them apart; today's bash envelope
 * carries one merged `output`, so `merged` is the field that fills. */
export type TerminalOutput = {
  stdout: string | null;
  stderr: string | null;
  merged: string | null;
  exitCode: number | null;
  timedOut: boolean | null;
};

/** The stream a single-block terminal draws: the merged capture when the
 * record kept one, else whichever stream it kept. Null when it kept none,
 * which is a shape that is not a terminal block. */
export function pickStream(output: TerminalOutput): string | null {
  return output.merged ?? output.stdout ?? output.stderr;
}

/** The object a call's text describes: the result envelope's `result` field
 * when it is an object, else the text itself (a tool answer that is its own
 * payload). Null when the text is not an object at all. */
export function payloadObject(node: JsonNode): JsonNode | null {
  if (node.kind !== "object") return null;
  for (const entry of node.entries) {
    if (entry.key === "result" && entry.node.kind === "object") {
      return entry.node;
    }
  }
  return node;
}

/** A parsed value as a plain value: what a validator reads. Numbers already
 * passed the safe-integer gate in `buildJsonTree`, so nothing rounds here. A
 * key named `__proto__` stays a key, since these objects carry no prototype. */
export function nodeValue(node: JsonNode): unknown {
  switch (node.kind) {
    case "object": {
      const value: Record<string, unknown> = Object.create(null);
      for (const entry of node.entries)
        value[entry.key] = nodeValue(entry.node);
      return value;
    }
    case "array":
      return node.items.map(nodeValue);
    case "string":
    case "number":
    case "boolean":
      return node.value;
    case "null":
      return null;
  }
}

/**
 * Whether an envelope says it failed: an `ok` key holding `false`. A tool that
 * never ran answers `{ok: false, error}`, which is a result like any other,
 * not a text that failed to parse. Callers ask this before `envelopeError`:
 * the two questions are separate, and a failure may carry no message at all.
 */
export function envelopeFailed(node: JsonNode): boolean {
  if (node.kind !== "object") return false;
  for (const entry of node.entries) {
    if (entry.key === "ok" && entry.node.kind === "boolean") {
      if (!entry.node.value) return true;
    }
  }
  return false;
}

/**
 * The first line of a failed envelope's `error`, or null when there is nothing
 * to say: either the envelope did not fail, or it failed without a usable
 * message. The row that shows it then supplies the wording, since an empty
 * error line would read as a rendering bug. Whether it failed is
 * `envelopeFailed`'s answer, never this function's.
 */
export function envelopeError(node: JsonNode): string | null {
  if (node.kind !== "object" || !envelopeFailed(node)) return null;
  let message = "";
  for (const entry of node.entries) {
    if (entry.key === "error" && entry.node.kind === "string") {
      message = entry.node.value;
    }
  }
  const head = (message.split("\n").at(0) ?? "").trim();
  return head === "" ? null : head;
}

/** A call the policy layer deferred for approval: the envelope says so and
 * names the approval. The id is optional on the wire, since the envelope a
 * failed serialisation leaves behind carries none
 * (crates/kallipai-adk/src/approval.rs). */
export type ApprovalPending = {
  /** The approval to commit or cancel, when the envelope names one. */
  id: string | null;
};

/**
 * The approval an envelope defers to, or null when it defers to none:
 * `pending_approval` holds `true`. The id is read only for a deferral, so a
 * success envelope that carries an `id` of its own is not mistaken for one.
 */
export function envelopePending(node: JsonNode): ApprovalPending | null {
  if (node.kind !== "object") return null;
  let pending = false;
  let id: string | null = null;
  for (const entry of node.entries) {
    if (entry.key === "pending_approval" && entry.node.kind === "boolean") {
      pending = entry.node.value;
    } else if (entry.key === "id" && entry.node.kind === "string") {
      id = entry.node.value;
    }
  }
  return pending ? { id } : null;
}

/** Read a terminal call's streams out of its payload. Every field is
 * optional on purpose: a record shape this build does not know yields nulls,
 * and the page falls back to the raw text rather than inventing a block. */
export function terminalOutput(node: JsonNode): TerminalOutput {
  const payload = payloadObject(node);
  const out: TerminalOutput = {
    stdout: null,
    stderr: null,
    merged: null,
    exitCode: null,
    timedOut: null,
  };
  if (payload === null || payload.kind !== "object") return out;
  for (const entry of payload.entries) {
    const value = entry.node;
    if (entry.key === "stdout" && value.kind === "string") {
      out.stdout = value.value;
    } else if (entry.key === "stderr" && value.kind === "string") {
      out.stderr = value.value;
    } else if (entry.key === "output" && value.kind === "string") {
      out.merged = value.value;
    } else if (entry.key === "exit_code" && value.kind === "number") {
      out.exitCode = value.value;
    } else if (entry.key === "timed_out" && value.kind === "boolean") {
      out.timedOut = value.value;
    }
  }
  return out;
}
/** The command a terminal block names: the `command` argument the tool ran,
 * or nothing. This is the one place that knows where a command lives, so a
 * view never has to guess a field name (a made-up stand-in would read as a
 * command the model never sent). */
export function terminalCommand(args: JsonNode | null): string | null {
  if (args === null || args.kind !== "object") return null;
  for (const entry of args.entries) {
    if (entry.key === "command" && entry.node.kind === "string") {
      return entry.node.value;
    }
  }
  return null;
}

/** A child node's fold key: the parent's key with one step appended,
 * encoded as a JSON pair so a key holding "." or "[" cannot make two
 * different paths share one string. */
export function childPath(parent: string, step: string | number): string {
  return JSON.stringify([parent, step]);
}

/** Lines a text holds, counted the way the face counts them (`str::lines`):
 * a trailing newline does not add an empty line, so a one-line answer plus
 * its newline is one line, not two. */
export function lineCount(text: string): number {
  if (text === "") return 0;
  const trimmed = text.endsWith("\n") ? text.slice(0, -1) : text;
  return trimmed.split("\n").length;
}

/** Whether a string value folds behind `Show all`. */
export function foldsAt(value: string): boolean {
  return (
    value.length > STRING_FOLD_CHARS || lineCount(value) > STRING_FOLD_LINES
  );
}

/** What a folded string shows: its first `STRING_FOLD_LINES` lines, then
 * its first `STRING_FOLD_CHARS` characters of those, ending in an ellipsis
 * so the cut is visible rather than implied. */
export function foldString(value: string): string {
  const lines = value.split("\n").slice(0, STRING_FOLD_LINES).join("\n");
  const cut =
    lines.length > STRING_FOLD_CHARS
      ? lines.slice(0, STRING_FOLD_CHARS)
      : lines;
  return `${cut}…`;
}
