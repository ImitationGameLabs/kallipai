// Pins for the failed line's delete action, each naming the regression it
// catches. The behaviour behind them is covered elsewhere: transcript_test and
// room-message_test for the reducers, session/delete_failed_test for the store,
// row_focus_test and reveal_test for the landing.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const VIEW = new URL("./ConversationView.svelte", import.meta.url);
const CHAT_PAGE = new URL("../pages/ChannelChatPage.svelte", import.meta.url);
const ROOM_PAGE = new URL(
  "../pages/RoomConversationPage.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the delete action sits behind its handler gate",
  { permissions: { read: [VIEW] } },
  () => {
    // Without the gate, DirectSessionPage (no handlers, no resend) grows a
    // destructive control on any cached failed line.
    const src = flat(source(VIEW));
    const branch = src.indexOf(
      '{#if line.role === "user" && line.status === "failed"}',
    );
    assert(branch !== -1, "the failed branch must exist");
    const next = src.indexOf("{:else}", branch);
    assert(next !== -1, "the failed branch must end before the shared bubble");
    const gate = src.indexOf("{#if onDeleteFailed}", branch);
    const button = src.indexOf("<DeleteFailedButton", branch);
    assert(
      gate !== -1 && gate < next,
      "the onDeleteFailed gate must sit inside the failed branch",
    );
    assert(
      button > gate && button < next,
      "the delete action must sit behind its gate, inside the failed branch",
    );
    assert(
      src.split("<DeleteFailedButton").length === 2,
      "one render site, and it is the gated one",
    );
  },
);

Deno.test(
  "the chat page wires delete for every conversation kind",
  { permissions: { read: [CHAT_PAGE] } },
  () => {
    // Copying the neighbouring retry expression (which branches on
    // OfflineConversation) type-checks and silently drops the offline half.
    const src = flat(source(CHAT_PAGE));
    const wiring = src.match(/onDeleteFailed=\{[^}]*(\}[^}]*)?\}/);
    assert(wiring !== null, "the page must wire onDeleteFailed");
    assert(
      !wiring[0].includes("OfflineConversation"),
      "the wiring must not branch on the conversation kind",
    );
    assert(
      wiring[0].includes("discard"),
      "the wiring must call the store's discard",
    );
  },
);

Deno.test(
  "the room page discards through its own store",
  { permissions: { read: [ROOM_PAGE] } },
  () => {
    // The room transcript is a separate class from the channel one, and its
    // discard is a filter with no durable row behind it. A copy of the
    // bilateral wiring would type-check only because both stores now answer to
    // the same name; the call still has to be the room's own.
    const src = flat(source(ROOM_PAGE));
    const branch = src.indexOf("{:else if line.mine && line.failed}");
    assert(branch !== -1, "the room failed branch must exist");
    const end = src.indexOf("{/if}", branch);
    const button = src.indexOf("<DeleteFailedButton", branch);
    assert(
      button > branch && button < end,
      "the discard must render inside the failed branch",
    );
    assert(
      src.slice(button, end).includes("discardRow(line."),
      "the bubble must route the discard through the page's handler",
    );
    const handler = src.indexOf("function discardRow(");
    assert(handler !== -1, "the room page must own the discard handler");
    assert(
      src
        .slice(handler, handler + 400)
        .includes("roomConversationsStore.discard"),
      "the room's own discard is what the handler calls",
    );
  },
);

Deno.test(
  "the channel view hands the landing the render index of the row that left",
  { permissions: { read: [VIEW] } },
  () => {
    // The store drops the line as it answers, so a list read afterwards no
    // longer holds it and a lookup by id answers with the first row of the
    // transcript: the reader is sent to the top, which also unsticks the
    // transcript from its tail. The row's own index survives both writes, so
    // this view must pass it rather than look the row up again.
    const src = flat(source(VIEW));
    assert(
      src.includes("onConfirm={() => discardRow(line.historyId, i)}"),
      "the channel view must hand its discard the row's render index",
    );
    assert(
      !src.includes("indexOf(removed)") && !src.includes("indexOf(seq)"),
      "the channel view must not look the departed row up in what remains",
    );
    assert(
      src.includes("reveal.land("),
      "the channel view must land through the one row table",
    );
  },
);

Deno.test(
  "the room page reads a discussion's place before the store drops the row",
  { permissions: { read: [ROOM_PAGE] } },
  () => {
    // The room transcript draws one entry per discussion, so a row's render
    // index no longer names its place. The page reads the place first and
    // only then lets the store drop the row: a lookup afterwards fails the
    // same way it always did, with nothing left to name and the reader sent
    // to the top of the transcript.
    const src = flat(source(ROOM_PAGE));
    const place = src.indexOf("const at = groups.findIndex(");
    const drop = src.indexOf("roomConversationsStore.discard(roomId, removed)");
    assert(
      src.includes("onConfirm={() => discardRow(line.seq)}"),
      "the room page wires its discard from the row that failed",
    );
    assert(
      place >= 0 && drop > place,
      "the room page must read the place before the store drops the row",
    );
    assert(
      !src.includes("indexOf(removed)") && !src.includes("indexOf(seq)"),
      "the room page must not look the departed row up in what remains",
    );
    assert(
      src.includes("reveal.land("),
      "the room page must land through the one row table",
    );
  },
);
