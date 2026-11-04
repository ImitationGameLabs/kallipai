// What a tool's own view is told, which tool names have one, and what an
// envelope says before either applies. The line this file keeps: a view is only
// ever handed a payload that already passed its validator, and a call that
// failed reaches no validator at all. The three rules below are why a validator
// cannot drift into a shape check of its own:
//
// 1. The payload must be an object: an array, a string or null has no fields to
//    read, and a view that guessed would draw a block the record never held.
// 2. Every field a view reads must be present and of the right type. `null` is
//    only accepted where the wire contract allows it, such as `exit_code` on a
//    signal death or on a command that became a background task.
// 3. A field a view reads only when it is there is type-checked when it
//    appears; a field nobody here knows about is ignored. That last part is
//    the point of the gate: a tool can grow a field and keep its view.
// 4. A view that answers for two tool names is told which one it was
//    registered under: the verb is the registry's answer, and a payload
//    cannot be asked for it without guessing.
import type { Component } from "svelte";
import type { NarrationTone } from "./turnView.ts";
import {
  BLOCK_FOLD_LINES,
  envelopeError,
  envelopeFailed,
  envelopePending,
  lineCount,
  nodeValue,
  payloadObject,
  type ApprovalPending,
  type JsonNode,
  type JsonTree,
} from "./jsonView.ts";

/** Everything a dedicated view is given. Views read nothing and fetch
 * nothing: the page owns the read, and the props are its result. */
export type CallViewProps<P> = {
  /** The tool name the registry matched. A view that answers two verbs
   * reads this instead of guessing one from the payload. */
  name: string;
  /** The call's arguments as a tree, or null when they did not parse. */
  args: JsonNode | null;
  /** The payload as a tree, for the readers that work on trees. */
  node: JsonNode;
  /** The same payload as a value, checked by the view's validator. */
  payload: P;
};

export type ToolView = {
  validate: (payload: unknown) => boolean;
  component: Component<CallViewProps<unknown>>;
};

/** Register a dedicated view. The narrow type stays on this line; a lookup by
 * name can only produce the erased shape. The erasure is safe because a true
 * answer from `validate` is what puts the payload behind it, and the two-leg
 * unit tests pin that. */
export function view<P>(
  validate: (value: unknown) => value is P,
  component: Component<CallViewProps<P>>,
): ToolView {
  return {
    validate,
    component: component as unknown as Component<CallViewProps<unknown>>,
  };
}

/** `bash_exec`'s result, as `BashExecOutput` serialises it
 * (crates/kallipai-shell/src/backend.rs). */
export interface BashExecResult {
  exit_code: number | null;
  timed_out: boolean;
  truncated: boolean;
  cwd: string;
  output?: string;
  stdout?: string;
  stderr?: string;
  task_id?: string;
}

/** `break`'s result: the park target the runtime actually took, defaults
 * included. `until` and `timeout_secs` are optional because records written
 * before the ack echoed them carry neither. */
export interface BreakResult {
  parked: boolean;
  until?: string;
  timeout_secs?: number;
}

/** `bash_background_read`'s result, as `BgReadOutput` serialises it
 * (crates/kallipai-shell/src/tools/bg_read.rs). */
export interface BgReadResult {
  task_id: string;
  output: string;
  /** `running` / `exited` / `killed` today; kept open on purpose, so a new
   * value keeps its view instead of dropping the whole record to the tree. */
  state: string;
  stalled: boolean;
  /** Bytes written so far, which is more than this read returned. */
  bytes: number;
  exit_code?: number;
  reason?: string;
}

/** `context_evict`'s result, as the tool's reply serialises it
 * (crates/kallipai-adk/src/tools/context/evict.rs). The window fields exist
 * only on records written by a build that reports them; older ones carry the
 * first three and are read the same way. */
export interface EvictResult {
  evicted: number;
  remaining_turns: number;
  freed_tokens: number;
  kept_turns?: number;
  max_turns?: number;
  max_tokens?: number;
  /** Why the token slice kept no turn; null when it kept the window. */
  degraded?: string | null;
}

/** `context_pin`'s result, as the tool's reply serialises it
 * (crates/kallipai-adk/src/tools/context/pin.rs). */
export interface ContextPinResult {
  pinned: string;
  /** Every label pinned after the call, in the order the store keeps them. */
  pinned_labels: string[];
}

/** `context_unpin`'s result, the same shape under the verb it answers with
 * (crates/kallipai-adk/src/tools/context/unpin.rs). */
export interface ContextUnpinResult {
  unpinned: string;
  /** Every label still pinned after the call, which may be none. */
  pinned_labels: string[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const isString = (value: unknown): value is string => typeof value === "string";
const isBoolean = (value: unknown): value is boolean =>
  typeof value === "boolean";
const isNumber = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);

const isStringOrNull = (value: unknown): value is string | null =>
  value === null || isString(value);

const isStringArray = (value: unknown): value is string[] =>
  Array.isArray(value) && value.every(isString);

/** Check only the optional fields that are present. */
function everyOptional(
  value: Record<string, unknown>,
  spec: Record<string, (field: unknown) => boolean>,
): boolean {
  for (const key of Object.keys(spec)) {
    if (!(key in value)) continue;
    const check = spec[key];
    if (check === undefined) continue;
    if (!check(value[key])) return false;
  }
  return true;
}

/** Check every required field: a missing or mistyped one fails. */
function everyRequired(
  value: Record<string, unknown>,
  spec: Record<string, (field: unknown) => boolean>,
): boolean {
  for (const key of Object.keys(spec)) {
    const check = spec[key];
    if (check === undefined) continue;
    if (!check(value[key])) return false;
  }
  return true;
}
export function isBashExecResult(value: unknown): value is BashExecResult {
  if (!isRecord(value)) return false;
  return (
    everyRequired(value, {
      exit_code: (field) => field === null || isNumber(field),
      timed_out: isBoolean,
      truncated: isBoolean,
      cwd: isString,
    }) &&
    everyOptional(value, {
      output: isString,
      stdout: isString,
      stderr: isString,
      task_id: isString,
    })
  );
}

export function isBreakResult(value: unknown): value is BreakResult {
  if (!isRecord(value)) return false;
  return (
    everyRequired(value, { parked: isBoolean }) &&
    everyOptional(value, { until: isString, timeout_secs: isNumber })
  );
}

export function isBgReadResult(value: unknown): value is BgReadResult {
  if (!isRecord(value)) return false;
  return (
    everyRequired(value, {
      task_id: isString,
      output: isString,
      state: isString,
      stalled: isBoolean,
      bytes: isNumber,
    }) && everyOptional(value, { exit_code: isNumber, reason: isString })
  );
}

export function isEvictResult(value: unknown): value is EvictResult {
  if (!isRecord(value)) return false;
  return (
    everyRequired(value, {
      evicted: isNumber,
      remaining_turns: isNumber,
      freed_tokens: isNumber,
    }) &&
    everyOptional(value, {
      kept_turns: isNumber,
      max_turns: isNumber,
      max_tokens: isNumber,
      degraded: isStringOrNull,
    })
  );
}

export function isContextPinResult(value: unknown): value is ContextPinResult {
  if (!isRecord(value)) return false;
  return everyRequired(value, {
    pinned: isString,
    pinned_labels: isStringArray,
  });
}

export function isContextUnpinResult(
  value: unknown,
): value is ContextUnpinResult {
  if (!isRecord(value)) return false;
  return everyRequired(value, {
    unpinned: isString,
    pinned_labels: isStringArray,
  });
}

/** A path short enough to end a sentence: its last two segments, with a
 * leading ellipsis when anything was dropped. A path already that short is
 * shown whole, so `/tmp` stays `/tmp` instead of `…/tmp`. */
export function shortPath(path: string): string {
  const parts = path.split("/").filter((part) => part !== "");
  if (parts.length <= 2) return path;
  return `…/${parts.slice(-2).join("/")}`;
}

/** A background task's id, cut to the head that fits one line. The stored
 * text keeps it whole; a line only has to identify it. */
export function shortId(id: string): string {
  return id.length <= 8 ? id : `${id.slice(0, 8)}…`;
}

/** A byte count at a readable scale, in the units the file list already
 * uses: a reader compares sizes, and a bare tally of half a million bytes
 * compares to nothing. A whole number keeps no decimal. */
export function formatBytes(bytes: number): string {
  const kb = Math.round((bytes / 1024) * 10) / 10;
  const mb = Math.round((bytes / (1024 * 1024)) * 10) / 10;
  const whole = (n: number) => (Number.isInteger(n) ? `${n}` : n.toFixed(1));
  if (bytes < 1024) return `${bytes} B`;
  // A value just under a megabyte rounds to 1024 KB, which is the next unit
  // already; a size is shown in the unit it has reached.
  return kb < 1024 ? `${whole(kb)} KB` : `${whole(mb)} MB`;
}

/** How a command ended, as the one fact a reader needs first. */
export type BashExecState = "succeeded" | "failed" | "running" | "no-code";

export type BashExecFacts = {
  state: BashExecState;
  exit: number | null;
  timedOut: boolean;
  truncated: boolean;
  cwd: string;
  taskId: string | null;
};

/** The facts a `bash_exec` result carries, as projection: the view places them,
 * it does not decide what they mean. The contract is `crates/kallipai-shell/
 * src/backend.rs:125-133`: `exit_code` is null on a signal death, and null
 * beside a `task_id` while a timed-out command keeps running in the
 * background. The one other null pair, `:513-522`, is a timeout killed after
 * its command could not be kept running. So an exit code that is null with
 * no task id is a failure this build cannot name a code for, never a run
 * still going. */
export function bashExecFacts(result: BashExecResult): BashExecFacts {
  const exit = result.exit_code;
  const taskId = result.task_id ?? null;
  const state: BashExecState =
    exit !== null
      ? exit === 0
        ? "succeeded"
        : "failed"
      : taskId === null
        ? "no-code"
        : "running";
  return {
    state,
    exit,
    timedOut: result.timed_out,
    truncated: result.truncated,
    cwd: result.cwd,
    taskId,
  };
}

/** How a parked turn will wake. A missing or unreadable `until` is its own
 * state: a record that never said how it parked is not a record that parked on
 * a timer, and neither is one that parked until the next message. */
export type BreakState =
  | "paused-timer"
  | "paused-message"
  | "paused-unrecorded"
  | "not-paused";

/** The wake timer in the unit it is worth saying: seconds below a minute,
 * whole minutes above it, rounded up so the line never promises a wake
 * earlier than the record holds. A five-second timer reported as "1 min"
 * would overstate it twelvefold, so a shorter one keeps its seconds. */
export type BreakTimer =
  | { unit: "seconds"; value: number }
  | { unit: "minutes"; value: number };

export type BreakFacts = {
  state: BreakState;
  timer: BreakTimer | null;
};

export function breakFacts(result: BreakResult): BreakFacts {
  if (!result.parked) return { state: "not-paused", timer: null };
  if (result.until === "wait") {
    const secs = result.timeout_secs ?? null;
    const timer: BreakTimer | null =
      secs === null
        ? null
        : secs < 60
          ? { unit: "seconds", value: Math.max(1, secs) }
          : { unit: "minutes", value: Math.max(1, Math.ceil(secs / 60)) };
    return { state: "paused-timer", timer };
  }
  if (result.until === "idle") {
    return { state: "paused-message", timer: null };
  }
  return { state: "paused-unrecorded", timer: null };
}

/** How a background task stands, in words a reader already has: still
 * writing, exited, killed, and anything this build does not know (shown with
 * the record's own word). */
export type BgReadState = "running" | "exited" | "killed" | "other";

export type BgReadFacts = {
  state: BgReadState;
  /** The record's state word, for the one case that shows it as written. */
  stateWord: string;
  exit: number | null;
  stalled: boolean;
  bytes: number;
  /** The lines this read returned: the tool answers with a tail. */
  tail: string;
  /** Whether the tail is long enough that the block folds it. */
  folded: boolean;
};

/** The facts one background read carries. An `exited` record without an
 * exit code is not the shape this view draws, so it keeps its word and falls
 * to the unknown arm rather than printing a code the record never held. The
 * record's `reason` is deliberately not read: it is also prefixed onto the
 * output text (crates/kallipai-shell/src/tools/bg_read.rs), so a line of its
 * own would say the same thing twice. */
export function bgReadFacts(result: BgReadResult): BgReadFacts {
  const known =
    result.state === "running" ||
    result.state === "killed" ||
    (result.state === "exited" && result.exit_code !== undefined);
  const state: BgReadState = !known ? "other" : (result.state as BgReadState);
  return {
    state,
    stateWord: result.state,
    exit: result.exit_code ?? null,
    stalled: result.stalled,
    bytes: result.bytes,
    tail: result.output,
    folded: lineCount(result.output) > BLOCK_FOLD_LINES,
  };
}

/** The dot a background read earns. It reports the state the sentence
 * names, not the alarm: a stalled task keeps its running line, and the line
 * under it carries the alarm, so the dot and the sentence never disagree. */
export function bgReadTone(facts: BgReadFacts): NarrationTone {
  switch (facts.state) {
    case "running":
      return "info";
    case "exited":
      return facts.exit === 0 ? "ok" : "error";
    case "killed":
      return "error";
    case "other":
      return "muted";
  }
}

export type EvictFacts = {
  evicted: number;
  /** The turns this call could condense: what it drained plus what the
   * keep window left. Pinned turns sit in neither count
   * (crates/kallipai-adk/src/context/store.rs: `remaining_turns` subtracts
   * them, and the drain stops where the partition they begin starts). */
  total: number;
  freed: number;
  /** The surviving window, only when the record states all three caps. */
  kept: {
    keptTurns: number;
    maxTurns: number;
    maxTokens: number;
  } | null;
  degraded: string | null;
  /** The configured turn cap is 0, which condenses every turn on purpose. */
  capZero: boolean;
};

/** The facts one eviction carries. The record's `note` is not read: the
 * two situations it explains (nothing to condense, a turn cap of 0) are
 * visible in the numbers, and reading its prose would make this file a
 * second copy of wording the tool owns. `first_kept_turn` is not read
 * either: a turn id inside a count reads as another count. */
export function evictFacts(result: EvictResult): EvictFacts {
  const kept =
    result.kept_turns !== undefined &&
    result.max_turns !== undefined &&
    result.max_tokens !== undefined
      ? {
          keptTurns: result.kept_turns,
          maxTurns: result.max_turns,
          maxTokens: result.max_tokens,
        }
      : null;
  return {
    evicted: result.evicted,
    total: result.evicted + result.remaining_turns,
    freed: result.freed_tokens,
    kept,
    degraded: result.degraded ?? null,
    capZero: kept !== null && kept.maxTurns === 0,
  };
}

/** A call that condensed nothing reports that nothing happened; one that
 * did reports a success. */
export function evictTone(facts: EvictFacts): NarrationTone {
  return facts.evicted === 0 ? "muted" : "ok";
}

/** How many labels a view prints as chips. Above it the chips are left out
 * whole: a row with no way to see the rest would say the set was smaller
 * than it is, so the sentence carries the count and no chips are drawn. */
const PIN_LABELS_ALL = 8;

export type ContextOpVerb = "pinned" | "unpinned";

export type ContextOpFacts = {
  verb: ContextOpVerb;
  /** The label this call named. */
  label: string;
  /** Every label still pinned, or none when there are too many to print. */
  labels: string[];
  total: number;
};

/** The facts one pin or unpin carries. Every such reply lists the whole
 * pinned set, so the view shows what the store holds instead of counting
 * forward from a reply it never saw. The verb is the name the registry
 * matched, not a field of the reply: a record carrying the other verb's
 * field would otherwise be read as that verb and print a value no
 * validator checked. */
export function contextOpFacts(
  name: string,
  result: ContextPinResult | ContextUnpinResult,
): ContextOpFacts {
  const verb: ContextOpVerb = name === "context_unpin" ? "unpinned" : "pinned";
  // The registry validated this payload under this name, so a cast to that
  // name's reply is what is in hand. Probing for a key would instead print
  // the other field's value.
  const label =
    verb === "unpinned"
      ? (result as ContextUnpinResult).unpinned
      : (result as ContextPinResult).pinned;
  const pinned = result.pinned_labels;
  return {
    verb,
    label,
    labels: pinned.length <= PIN_LABELS_ALL ? pinned : [],
    total: pinned.length,
  };
}

/** The four answers an envelope holds: a payload a validator may read, a
 * call deferred for approval, a failure, or a text that is not here yet.
 * The last one claims nothing about the record: a cut block and an empty
 * body are not results, and reading them as failures would say the tool
 * went wrong. */
export type ParsedEnvelope =
  | { kind: "payload"; payload: unknown; node: JsonNode }
  | { kind: "pending"; pending: ApprovalPending }
  | { kind: "failed"; error: string | null }
  | { kind: "unreadable" };

/**
 * The envelope, read once into the value a pick works on. Failure is asked
 * first and on its own: an envelope whose ok is false is a failed call
 * whether or not it carries a reason, and it never reaches a validator. The
 * reason is only wording, so a failure without one still fails here, and the
 * row that shows it supplies the words. A deferral is asked next: a deferred
 * call carries no result, and a tree of its envelope would show the approval
 * bookkeeping as if it were the tool's answer.
 */
export function parseEnvelope(node: JsonNode): ParsedEnvelope {
  if (envelopeFailed(node)) {
    return { kind: "failed", error: envelopeError(node) };
  }
  const pending = envelopePending(node);
  if (pending !== null) return { kind: "pending", pending };
  const payload = payloadObject(node) ?? node;
  return { kind: "payload", payload: nodeValue(payload), node: payload };
}

/** The same answer, taken from a tree that may not have parsed at all: a
 * text that is not here yet is answered as itself, not as a null every
 * caller has to know about. */
export function readEnvelope(tree: JsonTree): ParsedEnvelope {
  return tree.ok ? parseEnvelope(tree.node) : { kind: "unreadable" };
}

/** Four destinations, in this order: an unreadable envelope answers as
 * itself and keeps the shape chain, claiming nothing; a failure is stated in
 * words whatever the tool's name; a deferred call says it waits for
 * approval; a registered tool draws its own view once its payload checked
 * out. Everything else keeps the shape chain, which says a registered tool
 * fell back only once the text is here and the payload was rejected. */
export function pickView(
  name: string,
  parsed: ParsedEnvelope,
  views: Record<string, ToolView>,
): ViewPick {
  if (parsed.kind === "unreadable")
    return { kind: "generic", registered: false };
  if (parsed.kind === "failed") return { kind: "failed", error: parsed.error };
  if (parsed.kind === "pending") {
    return { kind: "pending", pending: parsed.pending };
  }
  const entry = Object.hasOwn(views, name) ? views[name] : undefined;
  if (entry === undefined) return { kind: "generic", registered: false };
  if (!entry.validate(parsed.payload)) {
    return { kind: "generic", registered: true };
  }
  return {
    kind: "dedicated",
    entry,
    payload: parsed.payload,
    node: parsed.node,
  };
}

export type ViewPick =
  | { kind: "pending"; pending: ApprovalPending }
  | {
      kind: "dedicated";
      entry: ToolView;
      payload: unknown;
      node: JsonNode;
    }
  | { kind: "failed"; error: string | null }
  | { kind: "generic"; registered: boolean };
