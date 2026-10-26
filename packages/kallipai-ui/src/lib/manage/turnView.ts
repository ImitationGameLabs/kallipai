// Pure decision layer for the turn-history card: what one read's answer
// carries -- state words, notice kinds, paging, truncation, reasoning
// folds and stored-text ranges. The card picks which state to show and
// owns the reads. The agent-and-era half of a still-belonging answer is
// `stillCurrent` here with its cases; the turn-identity half is the
// card's own call. A `.ts` module on purpose: `deno test` reads it
// without a rune shim.
//
// The shapes come from the tagma read face (kallipai-common
// `protocol::turns`). Two rules hold throughout:
//
// - A server fact is never re-worded: the card translates keys, it does not
//   re-derive counts, ranges or flags the face already states.
// - A failure is classified by which read asked and what status came back,
//   never by the tagma's prose. The turn routes answer five different 404s
//   (agent not found, no directory, no history log, no such turn, no such
//   call) with no machine-readable code, so matching sentences would weld
//   the UI to server wording.
//
// Three limits on what belongs here, stated so nobody over-reads it: a
// computed branch (a verdict, a range, a status word) belongs here; a raw
// presence check -- is `profile` there, is `effort` set, is `elapsed_ms`
// set, is `usage` there, is `text` non-null -- stays inline in the
// template, because it asks whether the wire carried a field at all; and
// picking which state to show is the card's own, pinned with the template
// branches it drives by the card's render test.
//
// zh wording: 思考 is the reasoning text, 推理 the token count. One English
// word covers both, so the split is a choice, not an oversight.

import { KallipaiError } from "@kallipai/kallipai-common";
import type {
  CallBody,
  CallRow,
  MessageRow,
  TurnRow,
  TurnsPage,
} from "@kallipai/kallipai-client";

/** The read-face version this build understands: hand-copied from
 * kallipai-common `TURN_SCHEMA_VERSION`, so a change there needs the same
 * change here. A face carrying a newer one is announced and not rendered --
 * the same stop the CLI makes, so two readers of one face never disagree
 * about whether it can be read. */
export const TURN_SCHEMA_VERSION = 1;

/** Whether this build can read a face at `version`. */
export function faceSupported(version: number): boolean {
  return version <= TURN_SCHEMA_VERSION;
}

/** Rows asked for on the first page: the server's own default, hand-copied
 * from kallipai-common `TURNS_PAGE_LIMIT_DEFAULT` (same change rule), and
 * deliberately not its maximum — the relay caps a response at 256 KiB while
 * the page already stops at 128 KiB. */
export const TURNS_PAGE_LIMIT = 50;

/** A remark the card translates: a discriminated kind, so a new one cannot be
 * added without the card's switch failing to compile. The card owns the
 * translation, so this layer stays free of i18n imports. */
export type Notice =
  | { kind: "bad-lines"; count: number }
  | { kind: "page-cut" }
  | { kind: "schema-newer" };

/** What a failed read means for the card. `no-log` is a 404 on the list read:
 * no history log, no such agent, or a tagma that serves no turn routes — the
 * face answers all three the same way, so the card cannot tell them apart.
 * `failed` is a read that broke. */
export type CardError = "none" | "no-log" | "failed";

/** Which read asked. A list 404 is the face saying it has nothing to give:
 * the three causes behind it are not the card's to sort out (see
 * `CardError`), while a detail 404 means the turn the list just named is
 * gone. */
export function errorState(
  face: "list" | "detail",
  status: number | undefined,
): CardError {
  if (status === undefined) return "failed";
  // Unreachable through either backend: both build a KallipaiError only from
  // a >= 400 answer. Kept as the last line of defence, with the risk stated:
  // a `none` here would render "no turns recorded" over a real failure.
  if (status < 400) return "none";
  if (face === "list" && status === 404) return "no-log";
  return "failed";
}

/** The HTTP status a caught failure carries, when it carries one. Anything
 * else (transport, decode) has no status and reads as `failed`. */
export function errorStatus(e: unknown): number | undefined {
  return e instanceof KallipaiError ? e.api.status : undefined;
}

/** Page-level remarks, in render order. A page from a newer face carries that
 * remark alone: the card does not render its rows (see `faceSupported`), so
 * counting its unreadable lines would describe rows nobody sees.
 * `earlierBadLines` is what the pages already walked reported: an unreadable
 * line is a fact about the log a reader has seen, so the count accumulates
 * instead of vanishing when an older page loads. */
export function pageNotices(page: TurnsPage, earlierBadLines = 0): Notice[] {
  if (!faceSupported(page.schema_version)) {
    return [{ kind: "schema-newer" }];
  }
  const notices: Notice[] = [];
  if (page.page_truncated) {
    notices.push({ kind: "page-cut" });
  }
  const badLines = earlierBadLines + page.bad_lines;
  if (badLines > 0) {
    notices.push({ kind: "bad-lines", count: badLines });
  }
  return notices;
}

/** Append an older page to the rows already shown. The face pages exclusively
 * at the cursor -- a row sitting on it is held back, not sent again -- so a
 * repeat should not occur; the guard keeps a doubled boundary row from ever
 * rendering twice, the cheap defence that survives a paging change. */
export function mergePages(
  rows: readonly TurnRow[],
  page: TurnsPage,
): TurnRow[] {
  const seen = new Set(rows.map((row) => String(row.turn_id)));
  const merged = [...rows];
  for (const row of page.rows) {
    const key = String(row.turn_id);
    if (seen.has(key)) continue;
    seen.add(key);
    merged.push(row);
  }
  return merged;
}

/** Whether "load older turns" is offered: the cursor is present (the server
 * omits it once the log is exhausted) and no read is in flight. A page cut
 * by the byte cap still carries a cursor, so the cap never hides the rest. */
export function canLoadMore(page: TurnsPage | null, loading: boolean): boolean {
  if (loading || page === null) return false;
  return page.next_cursor !== undefined && page.next_cursor !== "";
}

/** The one status word a call row shows. `ok` wins over a stale approval
 * flag: the tagma never emits that pair, and a deterministic answer beats a
 * branch nobody can reach. */
export function callStatus(call: CallRow): "ok" | "pending" | "failed" {
  if (call.ok) return "ok";
  if (call.pending_approval === true) return "pending";
  return "failed";
}

/** The facts a call row shows beside its status. A size pair is shown only
 * when the record carries both halves, and an exit code only when one
 * exists (`0` is a value, not an absence). */
export function callDetails(call: CallRow): {
  timedOut: boolean;
  cut: boolean;
  exit: number | null;
  stdout: { bytes: number; lines: number } | null;
  stderr: { bytes: number; lines: number } | null;
  firstError: string | null;
} {
  const pair = (bytes?: number, lines?: number) =>
    bytes !== undefined && lines !== undefined ? { bytes, lines } : null;
  const first = call.first_error_line;
  return {
    timedOut: call.timed_out === true,
    cut: call.truncated,
    exit: call.exit_code ?? null,
    stdout: pair(call.stdout_bytes, call.stdout_lines),
    stderr: pair(call.stderr_bytes, call.stderr_lines),
    firstError: first !== undefined && first !== "" ? first : null,
  };
}

/** A message row. `canReadFull` is the literal `false` on purpose: the read
 * face has no message-body route, so the only stored text is the shortened
 * head-and-tail a detail carries. Typing it as a literal makes a future
 * "read the whole message" button a compile error instead of a dead one. */
export function messageView(msg: MessageRow): {
  text: string | null;
  chars: number;
  cut: boolean;
  canReadFull: false;
} {
  return {
    text: msg.text ?? null,
    chars: msg.chars,
    cut: msg.truncated,
    canReadFull: false,
  };
}

/** The reasoning fold state of one message. `none` means the record carried
 * no reasoning at all, so no control is rendered; `unavailable` means the
 * flag is set but there is no text to show: every list row, a detail whose
 * record kept only the flag, and one whose text is blank. */
export function reasoningView(msg: MessageRow): {
  state: "none" | "text" | "unavailable";
  text: string | null;
  cut: boolean;
} {
  if (!msg.has_reasoning) return { state: "none", text: null, cut: false };
  const text = msg.reasoning;
  // Blank text is no text: the read face drops such a block, and the rule
  // that decides whether a fold opens says it again, so a face that missed
  // it still cannot open a fold on nothing.
  if (text === undefined || text === null || text.trim() === "") {
    return { state: "unavailable", text: null, cut: false };
  }
  return { state: "text", text, cut: msg.reasoning_truncated === true };
}

/** One range of a call's stored text. `to` is a byte offset: the face
 * measures bytes and widens each range to a character boundary, so the
 * range end is the start plus the encoded length of what arrived. A range
 * the face marks truncated without naming a next offset (a shape it never
 * emits) reports `more: null` with `atEnd: false`, so the card asks the
 * reader to reload instead of paging forever. */
export function bodyView(body: CallBody): {
  from: number;
  to: number;
  total: number;
  more: number | null;
  atEnd: boolean;
  empty: boolean;
  reclaimed: number;
} {
  const to = body.offset + new TextEncoder().encode(body.text).length;
  return {
    from: body.offset,
    to,
    total: body.total_bytes,
    more: body.truncated ? (body.next_offset ?? null) : null,
    atEnd: !body.truncated,
    empty: to === body.offset,
    reclaimed: body.reclaimed,
  };
}

/** One call's stored text as read so far: the ranges append, so `text` grows
 * while `from` keeps the first offset asked for. */
export type BodyText = {
  from: number;
  to: number;
  total: number;
  text: string;
  more: number | null;
  atEnd: boolean;
  empty: boolean;
  reclaimed: number;
};

/** The text a block starts from before its first range lands. */
export function emptyBodyText(): BodyText {
  return {
    from: 0,
    to: 0,
    total: 0,
    text: "",
    more: null,
    atEnd: false,
    empty: false,
    reclaimed: 0,
  };
}

/** Whether a read that started under `asked` may still write its answer: the
 * card still shows the agent it was asked for, and no id swap happened since.
 * The card bumps its epoch whenever it is re-pointed at another agent, so a
 * read from the agent shown a moment ago cannot land under the next one's
 * name -- even on the one route where the page is reused instead of mounted
 * anew. A body range asks a narrower question instead (a token the block
 * carries), because for it a closed block and a swapped agent are one case.
 */
export function stillCurrent(
  asked: { agent: string; epoch: number },
  current: { agent: string; epoch: number },
): boolean {
  return asked.agent === current.agent && asked.epoch === current.epoch;
}

/** Fold one more range into what is shown. `null` means this is the first
 * range: it sets the origin. An appended range keeps the first `from`, glues
 * its text on, and calls the block empty only when every range read so far
 * was empty -- "nothing in the whole text", not "nothing in this range". */
export function appendRange(
  previous: BodyText | null,
  read: CallBody,
): BodyText {
  const view = bodyView(read);
  if (previous === null) return { ...view, text: read.text };
  return {
    from: previous.from,
    to: view.to,
    total: view.total,
    text: previous.text + read.text,
    more: view.more,
    atEnd: view.atEnd,
    empty: previous.empty && view.empty,
    reclaimed: view.reclaimed,
  };
}
