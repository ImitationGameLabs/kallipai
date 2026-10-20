// Quote resolution across the chat surfaces: a quoted message travels as a row
// reference, never as a copy of its text. What a surface resolves it against
// differs -- the 1:1 transcript probes the target by id, the room and direct
// transcripts read it out of the window they hold -- so a target outside the
// loaded window (or deleted) renders as a placeholder instead of stale copy.
// The outcomes are kept apart -- `found`, `deleted`, `unavailable`,
// `off-window` -- because each one answers the same question differently: is
// there a row I can show?
//
//   - `found`       the target line is loaded.
//   - `deleted`     the tagma answered the window probe for that id with
//                   nothing at or below it in this conversation.
//                   Ids run on one global sequence, so an id that lives in
//                   another conversation reads the same way here: the fact
//                   claimed is absence from this partition, and the probe
//                   discloses nothing outside it.
//   - `unavailable` the probe failed (network, timeout, refusal) or has not run
//                   yet. The client cannot tell "not allowed to see this row"
//                   from "could not fetch it", so it does not invent the
//                   distinction: one retryable state, one message.
//   - `off-window`  a line quotes a seq the loaded window does not hold.
//                   Neither window surface has a probe to run -- a window
//                   moves by cursor, not by id -- so the absence is stated
//                   rather than resolved, and re-read on every render: a
//                   target that later lands in the window becomes `found`
//                   by itself.
//
// Pure and unit-tested in quote_test.ts -- no Svelte, no DOM.
//
// A probe is one `peekRow` read of the target id: `probe` in the prose here
// and `peek` in the controller name the same call.

import type { ConversationLine } from "./transcript.ts";

/** One quoted-message outcome as the bubble and the quote bar render it. */
export type QuoteStatus = "found" | "deleted" | "unavailable" | "off-window";

/** What the bubble needs to draw a quote block. `author` / `preview` are only
 *  present for `found`. */
export interface QuoteView {
  readonly status: QuoteStatus;
  readonly author?: string;
  /** Whether clicking the block can scroll to the target. Only a target the
   *  transcript already holds is navigable: a row outside the loaded window
   *  can be previewed (a probe read it) but not scrolled to, because
   *  the pager only moves older from the window floor -- paging a
   *  mid-history segment in would render a hole nothing can fill. */
  readonly jumpable?: boolean;
  readonly preview?: string;
}

/** Longest preview kept in memory (the block truncates again visually). */
export const QUOTE_PREVIEW_LIMIT = 120;

/** The minimum a line must expose to be previewed: one body plus an optional
 *  attachment whose file name stands in for an empty body. Every transcript
 *  surface meets it, so one shaper serves them all. */
export interface Previewable {
  readonly text: string;
  readonly attachment?: { readonly name: string } | undefined;
}

/** Collapse a message body to one display line: any whitespace run becomes a
 *  single space, then the tail is cut at `limit` characters with an ellipsis.
 *  An attachment-only message previews as its file name, so the block is never
 *  blank. */
export function previewOf(
  line: Previewable,
  limit = QUOTE_PREVIEW_LIMIT,
): string {
  const collapsed = line.text.replace(/\s+/g, " ").trim();
  const source =
    collapsed.length > 0 ? collapsed : (line.attachment?.name ?? "");
  if (source.length <= limit) return source;
  return `${source.slice(0, limit).trimEnd()}…`;
}

/** The loaded line a quote points at, if the transcript still holds it. Only
 *  positive ids are quotable: synthetic (negative) ids belong to optimistic and
 *  system lines that no message ever referenced. */
export function findQuotedLine(
  lines: readonly ConversationLine[],
  id: number,
): ConversationLine | undefined {
  if (!Number.isInteger(id) || id <= 0) return undefined;
  return lines.find((line) => line.historyId === id);
}

/** Build the block view from a loaded line. The author handle may be absent on
 *  rows cached before senders were tracked, and an empty handle names nobody,
 *  so both fold to no author at all: the block draws its unknown-author label
 *  rather than a blank line.
 *
 *  `jumpable` is false for a row a probe fetched rather than the window: it
 *  previews, but the row is not in the DOM to scroll to. Defaults to true,
 *  which is what a loaded line always is. */
export function quoteViewOfLine(
  line: ConversationLine,
  options: { readonly jumpable?: boolean } = {},
): QuoteView {
  return {
    status: "found",
    author: line.sender?.handle || undefined,
    jumpable: options.jumpable ?? true,
    preview: previewOf(line),
  };
}

/** The block view for a quote the loaded window does not resolve. */
export function unresolvedQuote(
  status: Exclude<QuoteStatus, "found">,
): QuoteView {
  return { status };
}

/** A quote armed by a transcript, tagged with the transcript that armed it. */
export interface ArmedQuote {
  /** The transcript that armed it: a room id, on a room surface. */
  readonly scope: string;
  /** The row seq armed within that transcript. */
  readonly seq: number;
}

/** The seq a read should act on: the armed one, only while its own transcript
 *  is still the one on screen. Every transcript numbers its rows from 1, so a
 *  seq armed in the transcript just left names a different row here. The quote
 *  bar, the preview lookup and the send all read through this one gate, so none
 *  of them can reach a row of a transcript the user has left. */
export function armedQuoteSeq(
  armed: ArmedQuote | null,
  scope: string,
): number | undefined {
  return armed !== null && armed.scope === scope ? armed.seq : undefined;
}

/** The minimum a loaded line must expose for a window quote lookup. */
export interface QuotableLine extends Previewable {
  /** The seq the surface assigned this line, unique within its transcript;
   *  negative for an optimistic or not-yet-reconciled line, which no quote
   *  can name. */
  readonly seq: number;
  /** The seq this line quotes, if the sender quoted one. */
  readonly quote?: number | undefined;
  /** The relay-stamped stable handle of the sender. */
  readonly senderHandle?: string | undefined;
}

/** How a surface files a quote block: the room surface files it under the
 *  line carrying the quote, the direct surface under the row that line quotes.
 *  Each surface looks up with the key it was handed. */
type QuoteKeyOf = (line: QuotableLine) => number | undefined;

/** Resolve every quote in a window in one pass, filing each block under
 *  `keyOf`'s key for the line carrying it. A loaded target yields `found`, an
 *  absent one `off-window`, and a line that quotes nothing files nothing.
 *
 *  One pass indexes the whole window because a room transcript has no
 *  ceiling: a history page is capped at 200 rows server-side, but the store
 *  appends every page and every live frame without trimming, so a long session
 *  renders thousands of rows. Indexing once per render keeps that linear; a
 *  lookup per row is quadratic in the same number. */
function quoteBlocksByKey(
  lines: readonly QuotableLine[],
  keyOf: QuoteKeyOf,
): Map<number, QuoteView> {
  const bySeq = new Map<number, QuotableLine>();
  for (const line of lines) bySeq.set(line.seq, line);
  const quotes = new Map<number, QuoteView>();
  for (const line of lines) {
    const key = keyOf(line);
    if (key === undefined) continue;
    const view = quoteBlockOf(bySeq, line);
    if (view !== undefined) quotes.set(key, view);
  }
  return quotes;
}

/** The quote blocks a room transcript draws, keyed by the seq of the line
 *  carrying them -- the same seq the room page renders that line under. */
export function roomQuoteBlocksByLine(
  lines: readonly QuotableLine[],
): Map<number, QuoteView> {
  return quoteBlocksByKey(lines, (line) => line.seq);
}

/** One line's block, resolved against an already-indexed window: a loaded
 *  target carries its author, preview and jump; an absent one reads as
 *  `off-window` and stays inert. The target is never fetched. */
function quoteBlockOf(
  bySeq: ReadonlyMap<number, QuotableLine>,
  line: QuotableLine,
): QuoteView | undefined {
  const seq = line.quote;
  if (seq === undefined || !Number.isInteger(seq) || seq <= 0) return undefined;
  const quoted = bySeq.get(seq);
  if (quoted === undefined) return unresolvedQuote("off-window");
  return {
    status: "found",
    author: quoted.senderHandle || undefined,
    jumpable: true,
    preview: previewOf(quoted),
  };
}

/** The quote blocks a direct-session page draws, keyed by the seq of the row
 *  being quoted, not by the seq of the line carrying the quote: the page hands
 *  the table to the bubble, which looks a block up with `line.quote`.
 *
 *  The transcript holds both the quoting row and its target, so this is the
 *  window lookup the room does, reached through the line shape this surface
 *  has: the direct transcript numbers its rows by the session's own seq, and
 *  only a confirmed row can be quoted. A quote of a row outside the window
 *  files an `off-window` block under that row's seq -- the page states the
 *  absence rather than probing for the row. */
export function directQuoteBlocksByTarget(
  lines: readonly ConversationLine[],
): Map<number, QuoteView> {
  return quoteBlocksByKey(lines.map(quotableOf), (line) => line.quote);
}

/** Project a transcript line onto the window-quote shape: the seq the session
 *  assigned it, the seq it quotes, the sender handle the transcript already
 *  resolved, and the body a preview is shaped from. */
function quotableOf(line: ConversationLine): QuotableLine {
  return {
    seq: line.historyId,
    text: line.text,
    quote: line.quote,
    senderHandle: line.sender?.handle,
    attachment: line.attachment,
  };
}
