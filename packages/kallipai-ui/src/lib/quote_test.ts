// Unit tests for quote resolution: the preview shaping, the loaded-window
// lookup, and the placeholder an unresolved target renders as.

import { assertEquals } from "@std/assert";
import {
  armedQuoteSeq,
  directQuoteBlocksByTarget,
  findQuotedLine,
  previewOf,
  QUOTE_PREVIEW_LIMIT,
  quoteViewOfLine,
  roomQuoteBlocksByLine,
  type QuotableLine,
  unresolvedQuote,
} from "./quote.ts";
import type { ConversationLine } from "./transcript.ts";

function line(over: Partial<ConversationLine>): ConversationLine {
  return {
    historyId: 1,
    role: "user",
    text: "hello",
    ...over,
  };
}

Deno.test("a preview collapses whitespace runs to one line", () => {
  const preview = previewOf(
    line({ text: "  first line\n\n second\t\tthird  " }),
  );
  assertEquals(preview, "first line second third");
});

Deno.test("an over-long preview is cut at the limit with an ellipsis", () => {
  const text = "x".repeat(QUOTE_PREVIEW_LIMIT + 40);
  const preview = previewOf(line({ text }));
  assertEquals(preview, `${"x".repeat(QUOTE_PREVIEW_LIMIT)}…`);
  assertEquals(previewOf(line({ text: "x".repeat(10) })), "x".repeat(10));
});

Deno.test("an attachment-only message previews as its file name", () => {
  const preview = previewOf(
    line({
      text: "   ",
      attachment: { record_id: "r1", name: "notes.pdf", size: 12 },
    }),
  );
  assertEquals(preview, "notes.pdf");
});

Deno.test("a quote resolves only against a positive, loaded id", () => {
  const lines = [
    line({ historyId: 7, text: "target" }),
    line({ historyId: -3 }),
  ];
  assertEquals(findQuotedLine(lines, 7)?.text, "target");
  assertEquals(findQuotedLine(lines, -3), undefined);
  assertEquals(findQuotedLine(lines, 0), undefined);
  assertEquals(findQuotedLine(lines, 99), undefined);
});

Deno.test("a found quote carries the author handle and the preview", () => {
  const view = quoteViewOfLine(
    line({
      historyId: 4,
      text: "quoted body",
      sender: { kind: "agent", id: "a1", handle: "ada" },
    }),
  );
  assertEquals(view, {
    status: "found",
    author: "ada",
    preview: "quoted body",
    jumpable: true,
  });
});

Deno.test("a quote of a row with no handle names no author", () => {
  // The operator sender is stamped with an empty handle, and a cached row can
  // carry no sender at all. Neither names an author, so the block draws its
  // unknown-author label instead of a blank line.
  assertEquals(
    quoteViewOfLine(
      line({
        historyId: 4,
        text: "quoted body",
        sender: { kind: "user", id: "op", handle: "" },
      }),
    ).author,
    undefined,
  );
  assertEquals(
    quoteViewOfLine(line({ historyId: 5, text: "quoted body" })).author,
    undefined,
  );
});

Deno.test("a probed row previews but cannot be jumped to", () => {
  const view = quoteViewOfLine(
    line({ historyId: 9, text: "outside the window" }),
    { jumpable: false },
  );
  assertEquals(view, {
    status: "found",
    author: undefined,
    preview: "outside the window",
    jumpable: false,
  });
});

Deno.test("an unresolved target speaks in facts, not failures", () => {
  assertEquals(unresolvedQuote("unavailable"), { status: "unavailable" });
  assertEquals(unresolvedQuote("deleted"), { status: "deleted" });
});

// --- the loaded window: a seq lookup over the lines already on screen ---

function quotable(over: Partial<QuotableLine>): QuotableLine {
  return { seq: 1, text: "hello", senderHandle: "@ada", ...over };
}

Deno.test("a line that quotes nothing draws no block", () => {
  const lines = [quotable({ seq: 1 }), quotable({ seq: 2 })];
  assertEquals(roomQuoteBlocksByLine(lines).size, 0);
});

Deno.test("a quote resolves against the loaded window", () => {
  const lines = [
    quotable({ seq: 4, text: "the original", senderHandle: "@ada" }),
    quotable({ seq: 9, text: "the reply", quote: 4 }),
  ];
  const quotes = roomQuoteBlocksByLine(lines);
  assertEquals(quotes.get(9), {
    status: "found",
    author: "@ada",
    jumpable: true,
    preview: "the original",
  });
  // The room looks a block up with the line it renders, not the row it names.
  assertEquals(quotes.get(4), undefined);
});

Deno.test("a quote previews an attachment-only target by its name", () => {
  const lines = [
    quotable({ seq: 4, text: "", attachment: { name: "spec.pdf" } }),
    quotable({ seq: 9, quote: 4 }),
  ];
  assertEquals(roomQuoteBlocksByLine(lines).get(9)?.preview, "spec.pdf");
});

Deno.test("a quote outside the window reads as absence", () => {
  // Nothing is fetched for it: the view states the absence and stays inert.
  const lines = [quotable({ seq: 9, text: "the reply", quote: 2 })];
  assertEquals(roomQuoteBlocksByLine(lines).get(9), { status: "off-window" });
});

Deno.test("a quote never resolves against a synthetic seq", () => {
  // Optimistic and unreconciled lines carry a negative seq, and a quote names
  // a lesche-assigned row, so neither side of a negative pair can match.
  const lines = [
    quotable({ seq: -1, text: "sending", quote: -1 }),
    quotable({ seq: 5, quote: -1 }),
  ];
  assertEquals(roomQuoteBlocksByLine(lines).size, 0);
});

Deno.test("the map holds only the lines that quote something", () => {
  const lines = [
    quotable({ seq: 1, text: "the original", senderHandle: "@ada" }),
    quotable({ seq: 2, text: "no quote here" }),
    quotable({ seq: 5, text: "the reply", quote: 1 }),
  ];
  const quotes = roomQuoteBlocksByLine(lines);
  assertEquals(quotes.size, 1);
  assertEquals(quotes.get(5)?.preview, "the original");
  assertEquals(quotes.get(1), undefined);
  assertEquals(quotes.get(2), undefined);
});

Deno.test("a quote names a row only when it is a positive integer", () => {
  const lines = [
    quotable({ seq: 9, text: "zero", quote: 0 }),
    quotable({ seq: 10, text: "fractional", quote: 1.5 }),
    quotable({ seq: 11, text: "negative", quote: -3 }),
  ];
  assertEquals(roomQuoteBlocksByLine(lines).size, 0);
});

Deno.test("a quote armed in one room is unreadable in another", () => {
  const armed = { scope: "room-a", seq: 7 };
  assertEquals(armedQuoteSeq(armed, "room-b"), undefined);
  assertEquals(armedQuoteSeq(armed, "room-a"), 7);
  assertEquals(armedQuoteSeq(null, "room-a"), undefined);
});

// --- the direct-session surface: the same lookup, another line shape ---

function directLine(over: Partial<ConversationLine>): ConversationLine {
  return { historyId: 1, role: "assistant", text: "hello", ...over };
}

Deno.test("a direct quote resolves against the loaded window", () => {
  const lines = [
    directLine({
      historyId: 4,
      text: "the original",
      sender: { kind: "agent", id: "peer", handle: "@peer" },
    }),
    directLine({ historyId: 9, text: "the reply", quote: 4 }),
  ];
  const quotes = directQuoteBlocksByTarget(lines);
  assertEquals(quotes.get(4), {
    status: "found",
    author: "@peer",
    jumpable: true,
    preview: "the original",
  });
  // The bubble looks a block up with the quoted row's seq, not the carrier's.
  assertEquals(quotes.get(9), undefined);
});

Deno.test("a direct quote outside the window reads as absence", () => {
  // The direct page holds a bounded window and has no by-id probe, so the
  // block states the absence instead of fetching the row.
  const lines = [directLine({ historyId: 9, text: "the reply", quote: 2 })];
  const quotes = directQuoteBlocksByTarget(lines);
  assertEquals(quotes.get(2), { status: "off-window" });
  assertEquals(quotes.get(9), undefined);
});

Deno.test("a chained quote describes the row it names", () => {
  const lines = [
    directLine({
      historyId: 1,
      text: "row one",
      sender: { kind: "agent", id: "a", handle: "@a" },
    }),
    directLine({
      historyId: 2,
      text: "row two",
      quote: 1,
      sender: { kind: "agent", id: "b", handle: "@b" },
    }),
    directLine({ historyId: 3, text: "row three", quote: 2 }),
  ];
  const quotes = directQuoteBlocksByTarget(lines);
  // The block filed under seq 2 describes row 2 itself, not the row row 2
  // quotes: a carrier-keyed table would hand back row one here.
  assertEquals(quotes.get(2), {
    status: "found",
    author: "@b",
    jumpable: true,
    preview: "row two",
  });
  assertEquals(quotes.get(1)?.author, "@a");
  assertEquals(quotes.get(3), undefined);
});

Deno.test("an unstamped handle reads as an unknown author", () => {
  // A relay row can arrive with an empty handle. An empty author is not a
  // nameless one: the block falls back to its unknown-author label.
  const lines = [
    quotable({ seq: 4, text: "the original", senderHandle: "" }),
    quotable({ seq: 9, quote: 4 }),
  ];
  assertEquals(roomQuoteBlocksByLine(lines).get(9)?.author, undefined);
});
