// What the room page draws for one message: a discussion the reader can see is
// a bubble that opens itself, a discussion whose head is out of the window
// keeps the plain button, and everything else is untouched.
//
// The decisions are unit-tested elsewhere (roomRowShape in threads_test.ts,
// bubbleActivates in bubble_click_test.ts). These pins hold the wiring that a
// type-check cannot see: which prop each shape passes, and that the summary is
// a real button inside the box rather than a click on a div pretending.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);
const BUBBLE = new URL("../components/MessageBubble.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "one verdict decides the shape, and nobody keeps a second copy",
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("{@const shape = roomRowShape(group)}"),
      "the row asks the shared verdict",
    );
    assert(
      !src.includes("group.replies.length > 0 ||"),
      "the template keeps no second copy of the rule",
    );
    assert(
      src.includes('{#if shape === "entry"}'),
      "the plain button is drawn for the entry shape alone",
    );
  },
);

Deno.test("a discussion row is a bubble that opens the discussion", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes('footer={shape === "thread" ? threadSummary : undefined}'),
    "the summary rides inside the bubble",
  );
  assert(
    src.includes('entry={shape === "thread"'),
    "one prop carries the entry: the click and the ring that announces it",
  );
  assert(
    src.includes('noActionRow={pending || shape === "thread"}'),
    "the discussion row keeps its actions in the panel",
  );
});

Deno.test("the summary is a button, and it lives inside the box", () => {
  const src = flat(source(PAGE));
  const open = src.indexOf("{#snippet threadSummary()}");
  const close = src.indexOf("{/snippet}", open);
  assert(open >= 0 && close > open, "the page declares the summary slot");
  const snippet = src.slice(open, close);
  assert(snippet.includes("<button"), "a control, not a click on the body");
  assert(
    snippet.includes("if (!selectionHoldsText(selectionAtClick(event))) {"),
    "it opens the same discussion the box does, unless the reader is copying",
  );
  assert(
    snippet.includes("{@render threadSummaryLines(false)}"),
    "the copy it draws is the one the entry button draws too",
  );
  assert(
    src.includes("{@render threadSummaryLines(true)}"),
    "the entry button asks for the stamp the footer leaves out",
  );

  const sharedOpen = src.indexOf("{#snippet threadSummaryLines(");
  const shared = src.slice(sharedOpen, src.indexOf("{/snippet}", sharedOpen));
  const builderOpen = src.indexOf("function threadSummaryText(");
  const builder = src.slice(builderOpen, src.indexOf("function closeThread("));
  assert(
    builder.includes("room_thread_last_from({") &&
      builder.includes("room_thread_replies_one({"),
    "one builder states the count and the last sender, as the entry always did",
  );
  assert(
    shared.includes("{words.head}") && shared.includes("{words.tail}"),
    "the entry draws the words that builder made",
  );
  assert(
    snippet.includes("aria-label={words.label}"),
    "the name a screen reader hears is the sentence the entry shows",
  );
  const previewOpen = snippet.indexOf("{#each preview.rows as row (row.seq)}");
  const previewRow = snippet.slice(
    previewOpen,
    snippet.indexOf("{/each}", previewOpen),
  );
  assert(previewOpen >= 0, "the preview rides in the same button");
  assert(
    previewRow.includes("line-clamp-2") && previewRow.includes("break-words"),
    "a preview row clamps to two lines and wraps a word too long for its box",
  );
  assert(
    previewRow.includes("class={PREVIEW_HANDLE}") &&
      previewRow.includes("class={PREVIEW_ROW}"),
    "the row and its handle wear the shared segments",
  );
  const classes = source(new URL("../lib/classes.ts", import.meta.url));
  assert(
    classes.includes("max-w-[50%]") &&
      classes.includes("truncate") &&
      classes.includes("gap-4") &&
      classes.includes("opacity-80"),
    "the cap and the spacing are stated once, where they were measured",
  );
  assert(
    previewRow.includes("title={row.senderHandle}"),
    "the name it cuts stays a hover away",
  );
  assert(
    snippet.indexOf("{row.text}") < snippet.indexOf("{row.senderHandle}"),
    "the body leads every row, so the replies share one left edge",
  );
  assert(
    !snippet.includes("border-t"),
    "no divider: the ring already bounds the block",
  );

  const bubble = flat(source(BUBBLE));
  const footer = bubble.indexOf("{@render footer?.()}");
  const row = bubble.indexOf("{#if !noActionRow}");
  assert(
    footer >= 0 && footer < row,
    "the summary renders inside the bubble, ahead of the action row",
  );
});

Deno.test("the body click goes through the tested decision", () => {
  const bubble = flat(source(BUBBLE));
  assert(
    bubble.includes(
      "if (bubbleActivates(body, selectionAtClick, box)) entry();",
    ),
    "the box asks the shared predicate",
  );
  assert(
    bubble.includes("event.target instanceof Element"),
    "a target that is no element reads as the body",
  );
  assert(
    !bubble.includes('role="button"'),
    "the box does not pass itself off as a control",
  );
  assert(
    source(BUBBLE).includes("a11y_click_events_have_key_events"),
    "the ignore names the reason the keyboard path lives elsewhere",
  );
});

Deno.test("the entry mark rides on the one prop that opens it", () => {
  const bubble = flat(source(BUBBLE));
  assert(
    bubble.includes("{entry ? BUBBLE_ENTRY_MARK : ''}"),
    "the box wears the entry ring wherever the surface passed an entry",
  );
  const classes = source(new URL("../lib/classes.ts", import.meta.url));
  assert(
    classes.includes(
      "cursor-pointer transition-shadow ring-1 ring-primary-800 dark:ring-primary-200 hover:ring-2",
    ),
    "the mark is a ring at rest, thickened on hover",
  );
});

Deno.test("a growth above the reader is handed back to the scroll", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes(
      "const aboveFoldPin = createAboveFoldPin(() => scroll.viewport);",
    ),
    "the page owns one growth pin for the whole transcript",
  );
  assert(
    src.includes(
      'use:holdAboveFold={shape === "thread" ? aboveFoldPin : undefined}',
    ),
    "only the row that can gain a reply preview takes the subscription",
  );
  assert(
    src.includes('pin={shape === "thread" ? undefined : togglePin}'),
    "a row takes the gesture's follow or the standing subscription, never both",
  );
  const bubble = flat(source(BUBBLE));
  assert(
    !bubble.includes("holdAbove"),
    "the bubble draws the preview and does not subscribe to its own height",
  );
});

Deno.test("the preview is asked for without the window gate", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("{@const preview = threadPreview(group)}"),
    "the preview asks about the discussion, not about the window",
  );
  assert(
    src.includes("{@const summary = threadEntry(group, countsKnown)}"),
    "the count still waits for the window to reach the present",
  );
});
