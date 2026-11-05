// Pure decision layer for the turn-history page: what one read's answer
// carries -- state words, notice kinds, paging, truncation, reasoning
// folds and stored-text ranges. The page picks which state to show and
// owns the reads. Both halves of a still-belonging answer live here:
// `stillCurrent` carries the agent-and-era half with its cases, and
// `bodyStillMine` carries the turn-and-block half. A `.ts` module on
// purpose: `deno test` reads it without a rune shim.
//
// The shapes come from the tagma read face (kallipai-common
// `protocol::history`). Two rules hold throughout:
//
// - A server fact is never re-worded: the page translates keys, it does not
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
// picking which state to show is the page's own, pinned with the template
// branches it drives by the page's render test -- except where a rule over
// the record decides what is drawable at all, which lives here, and the
// row reuses that answer (`turnShape` reads `visibleMessages`).
//
// zh wording: 思考 is the reasoning text, 推理 the token count. One English
// word covers both, so the split is a choice, not an oversight.

import { KallipaiError } from "@kallipai/kallipai-common";
import type {
  CallRow,
  CallPart,
  MessageRow,
  TurnRow,
  TurnsPage,
  TextPart,
} from "@kallipai/kallipai-client";

/** The read-face version this build reads: hand-copied from `kallipai-common`
 * `HISTORY_SCHEMA_VERSION`, so a change there needs the same change here. It has
 * to stay equal, because a page whose version differs from the record's would
 * read fields by the wrong meanings, so a face at another version is not
 * rendered; the CLI makes the same stop, and this number never labels a view. */
export const HISTORY_SCHEMA_VERSION = 3;

/** Whether this build can read a face at `version`. */
export function faceSupported(version: number): boolean {
  return version === HISTORY_SCHEMA_VERSION;
}

/** Rows asked for on the first page: the server's own default, hand-copied
 * from kallipai-common `TURNS_PAGE_LIMIT_DEFAULT` (same change rule), and
 * deliberately not its maximum — the relay caps a response at 256 KiB while
 * the page already stops at 128 KiB. */
export const TURNS_PAGE_LIMIT = 50;

/** A stored text as the face sends it: the text, and whether it is a prefix
 * whose rest is a byte-range read. Both come from the face, so no reader
 * infers a cut from the text's shape. */
export type HeldText = {
  text: string | null;
  more: boolean;
};

/** The byte offset a read continues from: the byte length of the text a
 * block already draws, in the measure the face pages by. A read asked from
 * zero would hand back the prefix on screen, so a block asks from here. */
export function prefixBytes(text: string | null): number {
  return text === null ? 0 : new TextEncoder().encode(text).length;
}

/** The text a block holds once a range has come back: the prefix it drew and
 * the range that continued it. One spelling for the box and for the copy
 * control, so neither can hand out the tail alone. */
export function wholeText(
  drawn: string | null,
  body: { text: string } | undefined,
): string {
  return `${drawn ?? ""}${body?.text ?? ""}`;
}

/** A call's arguments as one face sends them. `complete` says the text is
 * the whole argument and can be parsed as JSON at all: a page's row is a
 * summary whatever its length, so it is never reported as malformed JSON. */
export function argsView(
  call: CallRow,
  face: "page" | "detail",
): HeldText & { complete: boolean } {
  const more = call.args_more === true;
  return {
    text: call.args,
    more,
    complete: face === "detail" && !more,
  };
}

/** A remark the page translates: a discriminated kind, so a new one cannot be
 * added without the page's switch failing to compile. The page owns the
 * translation, so this layer stays free of i18n imports. */
export type Notice =
  | { kind: "bad-lines"; count: number }
  | { kind: "page-cut" }
  | { kind: "face"; face: number };

/** What a failed read means for the page. `no-log` is a 404 on the list read:
 * no history log, no such agent, or a tagma that serves no turn routes — the
 * face answers all three the same way, so the page cannot tell them apart.
 * `failed` is a read that broke. */
export type CardError = "none" | "no-log" | "failed";

/** Which read asked. A list 404 is the face saying it has nothing to give:
 * the three causes behind it are not the page's to sort out (see
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
 * remark alone: the page does not render its rows (see `faceSupported`), so
 * counting its unreadable lines would describe rows nobody sees.
 * `earlierBadLines` is what the pages already walked reported: an unreadable
 * line is a fact about the log a reader has seen, so the count accumulates
 * instead of vanishing when an older page loads. */
export function pageNotices(page: TurnsPage, earlierBadLines = 0): Notice[] {
  if (!faceSupported(page.schema_version)) {
    return [{ kind: "face", face: page.schema_version }];
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

/** The one status word a call row shows. A command's exit code is the verdict
 * that matters: a non-zero exit is a failed call even though the tool itself
 * answered, so the badge and the body cannot disagree about the same block.
 * `ok` otherwise wins over a stale approval flag: the tagma never emits that
 * pair, and a deterministic answer beats a branch nobody can reach. */
export function callStatus(call: CallRow): "ok" | "pending" | "failed" {
  const exit = call.exit_code ?? null;
  if (exit !== null && exit !== 0) return "failed";
  if (call.ok) return "ok";
  if (call.pending_approval === true) return "pending";
  return "failed";
}

/** The four tones a call's narration dot can take. */
export type NarrationTone = "ok" | "error" | "info" | "muted";

/** The class that paints one narration dot on the card (bg-surface-100-900).
 * Every paint is a paired shade -- one value in light mode, another in dark --
 * so each tone clears 3:1 against the card in both modes. The theme carries
 * no info hue, so a running call and a paused break share the neutral
 * shade. The fill says what happened, not what is still owed: filled, there
 * is a conclusion or something under way (a call running, a pause waiting to
 * be woken); a ring, nothing concluded and nothing happened (an eviction
 * that condensed nothing, a state this build cannot read). A break whose
 * words say nothing was parked takes the ring too: it is the one place
 * where the verdict and the ring say the same thing. */
export function toneDot(tone: NarrationTone): string {
  switch (tone) {
    case "ok":
      return "bg-success-800-200";
    case "error":
      return "bg-error-700-300";
    case "info":
      return "bg-surface-700-300";
    case "muted":
      return "border border-surface-700-300";
  }
}

/** The facts a call row shows beside its status. A size pair is shown only
 * when the record carries both halves, and an exit code only when one
 * exists (`0` is a value, not an absence). The error is the first line the
 * face kept; `more` says the rest is a body read of the `error` part, from
 * the byte this line ends at. */
export function callDetails(call: CallRow): {
  timedOut: boolean;
  cut: boolean;
  exit: number | null;
  stdout: { bytes: number; lines: number } | null;
  stderr: { bytes: number; lines: number } | null;
  /** The error's first line, held the way the arguments are: `more` covers
   * the tail of that line and any later lines alike, because one control
   * reads both on. */
  firstError: HeldText | null;
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
    firstError:
      first !== undefined && first !== ""
        ? { text: first, more: errorContinues(call) }
        : null,
  };
}

/** A message row's text as the face sends it. A prefix's rest is readable
 * through `GET /agents/{id}/history/{turn}/messages/{index}/body`, so the
 * page offers that read beside the text. */
export function messageView(msg: MessageRow): HeldText {
  return { text: msg.text ?? null, more: msg.text_more === true };
}

/** A text field as the record left it. `undefined` means the page did not
 * ask for the field, `null` means the record has none, and every face here
 * reads the two the same way: one rule decides for both. */
function keptText(value: string | null | undefined): string | null {
  return value ?? null;
}
/** Whether a text the record sent has anything to draw. A prefix of blanks is
 * still a prefix: the rest is one read away, so it is drawn as it is rather
 * than dropped. A whole text of blanks leaves nothing on screen. */
export function carriesText(text: string | null, more: boolean): boolean {
  if (more) return true;
  return text !== null && text.trim() !== "";
}
/** Whether the record's error holds more than the line a call draws: the
 * rest of the first line (the byte cap cut it), or later lines this face
 * keeps out of the row. Both are read through the body route's `error`
 * part, which starts at the offset the drawn line ends at. */
function errorContinues(call: CallRow): boolean {
  return call.error_more === true || (call.error_lines ?? 1) > 1;
}
/** The reasoning block of one message. `none` means the record carried no
 * reasoning at all, so no control is rendered; `unavailable` means the flag
 * is set but the text the record kept has nothing to draw: every list row,
 * a detail whose record kept only the flag, and one whose text is blank.
 * `text` holds the block the way the message text is, and `more` says the
 * rest is a body read of the reasoning part. */
export function reasoningView(
  msg: MessageRow,
): { state: "none" | "text" | "unavailable" } & HeldText {
  const blank: HeldText = { text: null, more: false };
  if (!msg.has_reasoning) return { state: "none", ...blank };
  const text = keptText(msg.reasoning);
  // A blank text is no block: the read face drops such a block, and a face
  // that missed it still cannot open a fold on nothing.
  if (!carriesText(text, msg.reasoning_more === true)) {
    return { state: "unavailable", ...blank };
  }
  return { state: "text", text, more: msg.reasoning_more === true };
}

/** What a turn was, asked of its own record: a round that ran tools first
 * (that is what a reader trips over, and the model's own words around the
 * call are drawn beside them), then the roles that remain. `none` means no
 * role this page names: a system message still draws its role line, so the
 * row says nothing while the pane says what it can. */
export type TurnShape = "user" | "tools" | "assistant" | "none";

/** One message as the detail pane draws it: the message and the index its
 * body reads are keyed on. The face reads a message body by its position in
 * the turn, so a filter that dropped the index would read another message's
 * text. */
export type MessageSlot = { index: number; message: MessageRow };

/** Whether the turn ran a tool. The row, the pane and the call section all ask
 * this, so what counts as a tool round is decided here once. */
export function ranTools(turn: TurnRow): boolean {
  return turn.calls.length > 0;
}

/** Whether an assistant message carries text worth a block. The read face
 * drops an all-blank reasoning text at the source (`history/view.rs` keeps
 * it only when `!trim().is_empty()`) but counts a text exactly as stored,
 * whitespace included -- that asymmetry is why this side trims. With no
 * text sent (a list page) the count is the only evidence left. */
function hasText(msg: MessageRow): boolean {
  const text = keptText(msg.text);
  if (text === null) return msg.chars > 0;
  return carriesText(text, msg.text_more === true);
}

/** The messages worth drawing in the detail pane. A tool result is drawn by
 * its call's own view (the same text, laid out), and an assistant message
 * with no text and no reasoning text has nothing to draw. Everything else
 * stays, unknown roles included: a record this page cannot name is still a
 * record. A user message that carried only an image has no text, so its block
 * is a bare role line; a record of that shape may still arrive, and dropping
 * it would hide the one thing the record says.
 *
 * A record with no call keeps its tool messages: nothing else would draw
 * them. A record with calls trusts each call view to carry the tool messages
 * it ran; a record that kept more tool messages than calls would lose the
 * extra bodies here, a shape no record has shown so far.
 *
 * The row and this pane answer from different evidence: a list page carries
 * counts, a detail carries text. The row speaks when it has a word to say,
 * and the faces disagree in both directions. A text blank throughout: the
 * read face counts it and does not drop it, while this side trims it away,
 * so the row says Assistant and the pane draws no block. A reply carrying
 * only reasoning text: a list page is not sent the reasoning, so the row has
 * no word, while the pane draws the reasoning block. Roles this page does
 * not name (a system message, a tool message kept without its call) draw in
 * the pane while the row says nothing. */
export function visibleMessages(turn: TurnRow): MessageSlot[] {
  const drawn = ranTools(turn);
  return turn.messages
    .map((message, index) => ({ index, message }))
    .filter(({ message }) => {
      if (message.role === "tool") return !drawn;
      if (message.role === "assistant") {
        // Blank text is no text (reasoningView says why), so an assistant
        // message with neither text nor readable reasoning has no block.
        return hasText(message) || reasoningView(message).state === "text";
      }
      return true;
    });
}

/** The turn's shape, read off the messages the pane actually draws: a call
 * wins over the roles (a round that ran tools is a tool round even when it
 * also carries the message that asked for it), and a role counts only when
 * its block survives `visibleMessages`. The row answers from the page face
 * (counts) and the pane from the detail face (prose), so the two part in
 * both directions; `visibleMessages` says which shapes do it. */
export function turnShape(turn: TurnRow): TurnShape {
  if (ranTools(turn)) return "tools";
  const slots = visibleMessages(turn);
  if (slots.some(({ message }) => message.role === "user")) return "user";
  if (slots.some(({ message }) => message.role === "assistant")) {
    return "assistant";
  }
  return "none";
}

/** One range of stored text, from either body read: a call body and a
 * message body carry the same paging fields, so one view serves both.
 *
 * `to` is a byte offset: the face measures bytes and widens each range to
 * a character boundary, so the range end is the start plus the encoded
 * length of what arrived. A range the face marks truncated without naming
 * a next offset (a shape it never emits) reports `more: null` with
 * `atEnd: false`, so the page asks the reader to reload instead of paging
 * forever. */
export type TextRange = {
  offset: number;
  total_bytes: number;
  text: string;
  truncated: boolean;
  next_offset?: number;
  reclaimed: number;
};

export function bodyView(body: TextRange): {
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

/** One call's stored text as the page holds it: the ranges read so far,
 * plus the states only the reader knows. The token a read carries stays
 * with the page -- it is identity, never rendered. */
export type BodyState = BodyText & {
  loading: boolean;
  error: string | null;
  /** The face version this build cannot read; null when it can. */
  unsupported: number | null;
};

/** A slot inside one turn: the turn id alone repeats across turns, the index
 * repeats across turns too, and one row carries more than one stored text,
 * so all three parts go into the key. A call's parts and a message's parts
 * have different names, so one key space holds both without a collision. */
function slotKey(turnId: number, index: number, part: string): string {
  return `${turnId}:${index}:${part}`;
}

/** Where one part's text is held: the turn, the row's index inside it, and
 * which part. The index counts calls on a call row and messages on a message
 * row; the part names belong to one family or the other, so the two never
 * land in the same slot. */
export function bodyKey(
  turnId: number,
  index: number,
  part: CallPart | TextPart = "result",
): string {
  return slotKey(turnId, index, part);
}

/** Whether one reasoning block is open, keyed by the block's index. */
export function reasoningKey(turnId: number, blockIndex: number): string {
  return slotKey(turnId, blockIndex, "open");
}

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

/** One call's text as the page holds it: what a block shows, the states
 * only the reader knows, and the token of the read filling it. The token is
 * never rendered; it is the identity an answer compares before it may write. */
export type BodyEntry = BodyState & { token: number };

/** The entry a read writes when it starts. Two properties, both
 * load-bearing: the ranges read so far stay on screen while the next one is
 * in flight, and the entry carries THIS read's token. An answer compares that
 * stamp against the token it started under, so a read that forgets to stamp
 * can never be recognised as its own and every answer lands nowhere. */
export function startRead(
  previous: BodyEntry | null,
  token: number,
): BodyEntry {
  const base: BodyEntry = previous ?? {
    ...emptyBodyText(),
    loading: true,
    error: null,
    unsupported: null,
    token: 0,
  };
  return { ...base, loading: true, error: null, token };
}

/** Whether an answer started under `token` still owns the block: the entry
 * on screen must be the one that read stamped. The type arm is the point: a
 * caller that checks this has, to the compiler, a stamped entry, so no cast
 * is needed after it. */
export function bodyStillMine(
  entry: BodyEntry | undefined,
  token: number,
): entry is BodyEntry {
  return entry !== undefined && entry.token === token;
}

/** What one read turned out to be. */
export type BodyOutcome =
  | { kind: "text"; text: BodyText }
  | { kind: "unsupported"; face: number }
  | { kind: "failed"; error: string };

/** Write an answer into the entry its read started. The stamp stays: it
 * names the read that filled the block, which is what the next answer
 * compares itself to. */
export function finishRead(entry: BodyEntry, outcome: BodyOutcome): BodyEntry {
  if (outcome.kind === "unsupported") {
    return { ...entry, loading: false, error: null, unsupported: outcome.face };
  }
  if (outcome.kind === "failed") {
    return { ...entry, loading: false, error: outcome.error };
  }
  return {
    ...entry,
    ...outcome.text,
    loading: false,
    error: null,
    unsupported: null,
  };
}

/** How many calls of a turn that just opened have their result read
 * without being asked. A turn can carry dozens of calls, and each read is
 * its own request, so the first few open by themselves; the rest stay one
 * click away, and a reader who needs the eleventh asks for it.
 */
export const AUTO_READ_CALLS = 10;

/** The calls of a turn whose stored result nobody has asked for: no read in
 * flight, nothing back, and no failure waiting for a retry. A block carrying
 * any of those keeps what it has, because reading it again would repeat the
 * request or throw away the answer on screen. Only the first `limit` calls
 * are looked at, and what comes back is their call indexes in the order the
 * turn ran them, not the results themselves. */
export function untouchedResults(
  turn: TurnRow,
  held: Readonly<Record<string, BodyEntry>>,
  limit: number = AUTO_READ_CALLS,
): number[] {
  return turn.calls
    .slice(0, limit)
    .map((call) => call.call_index)
    .filter(
      (index) => held[bodyKey(turn.turn_id, index, "result")] === undefined,
    );
}

/** Whether a read that started under `asked` may still write its answer: the
 * page still shows the agent it was asked for, and no id swap happened since.
 * The page bumps its epoch whenever it is re-pointed at another agent, so a
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
 * was empty -- "nothing in the whole text", not "nothing in this range".
 *
 * An appended range must begin exactly where the last one ended. A range
 * that does not would splice two stretches of the text into one string
 * that exists nowhere in the log, so this returns null instead and the
 * page reports a failed read. */
export function appendRange(
  previous: BodyText | null,
  read: TextRange,
): BodyText | null {
  const view = bodyView(read);
  if (previous === null) return { ...view, text: read.text };
  // A gap or an overlap is not a join: refuse it rather than glue.
  if (read.offset !== previous.to) return null;
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

/** The turn on show, and whether the layout rather than the reader chose it.
 * The mark is what lets a pick of the layout's be undone when the second
 * column goes away; a reader's pick has none and is never undone. */
export type ShownTurn = { turnId: number | null; auto: boolean };

/** Nothing on show: where a page starts, and what dropping a pick the layout
 * made returns to. */
export const NO_TURN: ShownTurn = { turnId: null, auto: false };

/** The turn to show once the rows land, or once the room for a second column
 * appears: the newest one, while both columns are on and nobody has chosen.
 * A pick already on show is returned as it is, whoever made it: a refresh
 * keeps a reader's pick and the layout's own alike.
 */
export function showOnRows(input: {
  panes: boolean;
  shown: ShownTurn;
  rows: readonly TurnRow[];
}): ShownTurn {
  if (!input.panes) return input.shown;
  if (input.shown.turnId !== null) return input.shown;
  const newest = input.rows[0];
  if (newest === undefined) return input.shown;
  return { turnId: newest.turn_id, auto: true };
}

/** The turn to show once the width changes: losing the second column drops a
 * pick the layout made, because the stacked page is the list the reader came
 * back for, and keeps a pick the reader made. Gaining it opens the newest turn
 * the way the rows landing does. */
export function showOnWidth(input: {
  panes: boolean;
  shown: ShownTurn;
  rows: readonly TurnRow[];
}): ShownTurn {
  if (!input.panes) return input.shown.auto ? NO_TURN : input.shown;
  return showOnRows(input);
}
