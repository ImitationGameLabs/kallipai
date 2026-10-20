// Source-read pins for the room page's quoting wiring. The lookup itself is
// behaviour-tested (quote_test), and the block's rendering is the 1:1 one; what
// is left is the page, where the wiring can quietly go missing: a resolved view
// that never reaches the bubble, a jump offered for a row that is not there, or
// an absent target that starts fetching.

import { assert, assertEquals } from "@std/assert";
import { blockAt } from "../test_support/block.ts";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);

const STORE = new URL(
  "../lib/session/roomConversations.svelte.ts",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the room page resolves the window's quotes in one pass", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("roomQuoteBlocksByLine("),
    "one pass over the loaded window, not one lookup per row",
  );
  assert(
    !src.includes("conv.lines, line"),
    "no per-row scan of the whole transcript",
  );
  assert(
    src.includes("{@const q = quotes.get(line.seq)}"),
    "the resolved view reaches each bubble by its seq",
  );
  assert(src.includes("quote={q}"), "the block draws from that view");
});

Deno.test("the page tears the reveal controller down", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("$effect(() => () => {");
  assert(at !== -1, "the page has a teardown effect");
  const block = blockAt(src, at);
  assert(
    block.includes("reveal.destroy();"),
    "the flash timer stops when the page unmounts",
  );
  assert(
    block.includes("rowActions.reset();"),
    "and the row controller goes with it, not through a session swap",
  );
});

Deno.test("the jump is offered only for a target in the window", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes(
      "{@const quoteTarget = q?.jumpable === true ? quotedSeq : undefined}",
    ),
    "one judgement decides whether a target has a row",
  );
  assert(
    src.includes("quoteDescribedBy={quoteTarget === undefined") &&
      src.includes("onQuoteClick={quoteTarget === undefined"),
    "the spoken target and the click read it, so they cannot drift",
  );
  assert(
    src.includes("reveal.jump(quoteTarget)"),
    "the jump scrolls to the quoted seq",
  );
});

Deno.test("rows register and mark by room seq", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("use:reveal.trackRow={line.seq}"),
    "the reveal table is keyed by seq",
  );
  assert(
    src.includes("revealMark(reveal.highlightId, line.seq)"),
    "the mark follows the same key",
  );
});

Deno.test("a jump speaks the row it lands on", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("const reveal = createReveal({");
  assert(at !== -1, "the page builds the reveal controller");
  const controller = blockAt(src, at);
  assert(
    controller.includes("describe: (seq) =>"),
    "the page answers what the row it landed on is called",
  );
  assert(
    controller.includes("if (line === undefined) return undefined;"),
    "a seq the window does not hold is not named at all",
  );
  assert(
    controller.includes("return roomRowAnnouncement(line, {"),
    "the sentence is assembled in the shared room-message module",
  );
  assert(
    controller.includes("time: formatStamp(line.createdAt, ROW_TIME),"),
    "and it carries the row's own time",
  );
  assert(
    controller.includes("authorLabel: memberById.get(line.senderId)?.label"),
    "naming the author the row's own label names",
  );
  const row = src.indexOf("use:reveal.trackRow={line.seq}");
  assert(row !== -1, "the row registers itself with the reveal table");
  const rowTag = src.slice(row, row + 400);
  assert(
    rowTag.includes('tabindex="-1"'),
    "the row a jump lands on can take the focus",
  );
});

Deno.test("the jump sentence reaches a standing region", () => {
  const src = flat(source(PAGE));
  const regions = src.match(/aria-live="polite"/gu) ?? [];
  assertEquals(regions.length, 2, "the page keeps two regions, one per slot");
  for (const slot of [0, 1]) {
    assert(
      src.includes(
        `{reveal.slot === ${slot} ? (reveal.announcement ?? "") : ""}`,
      ),
      `slot ${slot} carries the sentence when it is that slot's turn`,
    );
  }
  assert(
    !src.includes("{reveal.announcement}"),
    "one node would repeat the same sentence silently",
  );
});

Deno.test("a row's anchor and the quote block name the same row", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes('<span id={roomRowAnchorId(line.seq)} class="sr-only">'),
    "each row stamps its own seq as the anchor a quote points at",
  );
  assert(
    src.includes("roomRowAnchorId(quoteTarget)"),
    "and the quote block points at the target row's anchor",
  );
});

Deno.test("a room change drops the sentence it spoke", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("void roomId;");
  assert(at !== -1, "the page has one effect that owns the room swap");
  const effect = blockAt(src, at);
  assert(
    effect.includes("return () => reveal.destroy();"),
    "the sentence spoken in the room that left goes with it",
  );
  assert(
    !effect.replace("return () => reveal.destroy();", "").includes("destroy"),
    "the body does not fire the reset itself",
  );
  assert(
    !src.includes("reveal.destroy(); untrack(() => unreadStore.leaveRoom("),
    "the open effect's cleanup is not the room-swap path",
  );
});

Deno.test("the armed quote is read through its own room", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("armedQuoteSeq(pendingQuote, roomId)"),
    "one gate decides whether the armed seq is readable here",
  );
  assert(
    !src.includes("line.seq === pendingQuote"),
    "no read of the armed seq without that gate",
  );
  assert(
    src.includes("{#if armedSeq !== undefined}"),
    "the quote bar draws only for a seq this room can read",
  );
});

Deno.test("a quote never adds a fetch to the page", () => {
  const src = flat(source(PAGE));
  assert(
    !src.includes("fetchRoomMessages"),
    "the page reads the store, never the wire",
  );
  assert(!src.includes("peekRow"), "a room surface has no probe to fire");
  const refreshes = src.match(/roomConversationsStore\.refresh\(/gu) ?? [];
  assertEquals(refreshes.length, 1, "one poll refresh, unchanged by quoting");
});

Deno.test(
  "the store builds every room line through the shared function",
  () => {
    const src = flat(source(STORE));
    assert(
      src.includes("roomLineFrom("),
      "the decoded payload becomes a line in one place",
    );
    assert(
      !src.includes("senderId: sender.id"),
      "no hand-rolled line object that could drop a field again",
    );
    assert(
      !src.includes("const line: RoomLine = {"),
      "no inline line literal either",
    );
  },
);

Deno.test("the page hands each controller its own room", () => {
  const src = flat(source(PAGE));

  const rows = src.indexOf("const rowActions = createRowActions<RoomLine>({");
  assert(rows !== -1, "the row controller is built on the page");
  const rowWiring = src.slice(rows, rows + 320);
  for (const line of [
    "idOf: (line) => line.seq,",
    "onQuote: (line) => (pendingQuote = { scope: roomId, seq: line.seq }),",
    "scope: () => roomId",
  ]) {
    assert(rowWiring.includes(line), `row controller wiring: ${line}`);
  }

  const picker = src.indexOf("const picker = createMentionPicker({");
  assert(picker !== -1, "the picker is built on the page");
  const pickerWiring = src.slice(picker, picker + 320);
  for (const line of [
    "scope: () => roomId,",
    "members: () => conv?.roster?.members ?? [],",
    "dropToken: (start, end) => {",
  ]) {
    assert(pickerWiring.includes(line), `picker wiring: ${line}`);
  }
  assert(
    src.includes("onPick={(id) => picker.pick(id)}"),
    "the popup's pick reaches the controller instead of a stub",
  );
  assert(
    src.includes("composer.requestCaret(start)"),
    "accepting a candidate puts the caret back where the token was",
  );
});

Deno.test("the affordance is drawn only for a row the room can quote", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("{#if quotable(line)}"),
    "an optimistic or failed row offers no quote entry",
  );
  assert(
    src.includes('side={line.mine ? "right" : "left"}'),
    "the side follows the row that drew this bubble",
  );
});

Deno.test("a room change drops the row controller with the scroll", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("void roomId;");
  assert(at !== -1, "the room's session effect reads the route");
  const effect = blockAt(src, at);
  assert(
    effect.includes("scroll.reset();") &&
      effect.includes("rowActions.reset();"),
    "the host's session effect drops the menu, not the scope check",
  );
});

Deno.test("the key hook asks the picker before anything else", () => {
  const src = flat(source(PAGE));
  const at = src.indexOf("function onComposeKey(");
  assert(at !== -1, "the page owns the composer's key hook");
  const body = src.slice(src.indexOf("{", at) + 1);
  assert(
    body.trimStart().startsWith("if (picker.key(event)) {"),
    "the picker's predicate is the first thing the hook reads",
  );
});
