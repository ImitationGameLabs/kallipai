import { assert } from "@std/assert";
import { codeLines } from "../lib/testing/source_pins.ts";

// Wiring pin for the channel page's error surface. The rule itself is a pure
// function with behavior tests (errorSurface_test.ts), and the composer's note
// dispatch has its own (composerState_test.ts); what no unit test can see is
// the page calling the rule, handing the result to the view, and being the one
// page that reports a channel's failure.
const PAGE = new URL("./ChannelChatPage.svelte", import.meta.url);
const TAGMA_PAGE = new URL("./TagmaChatPage.svelte", import.meta.url);
const COMPOSER = new URL("../components/Composer.svelte", import.meta.url);

Deno.test("the page builds its surface through the shared rule", () => {
  const src = codeLines(PAGE);
  assert(
    src.includes(
      'import { errorSurface } from "../lib/session/conversation/errorSurface.ts";',
    ),
  );
  assert(src.includes("const surface = $derived("));
  assert(src.includes("errorSurface({"));
  assert(src.includes("causeSpokenAbove: isLocal"));
  assert(src.includes("status={surface.status}"));
  assert(src.includes("error={surface.error}"));
});

Deno.test("the page classifies a link failure and hands it to the rule", () => {
  const src = codeLines(PAGE);
  const start = src.indexOf("const linkCopy = $derived(");
  const end = src.indexOf(");", start);
  assert(start >= 0 && end > start);
  const block = src.slice(start, end);
  assert(block.includes("classifyError(linkError).title"));
  assert(block.includes("isWriteRefusal(linkError)"));
  const callAt = src.indexOf("errorSurface({");
  assert(callAt > 0);
  const callEnd = src.indexOf("}),", callAt);
  assert(callEnd > callAt);
  assert(src.slice(callAt, callEnd).includes("linkCopy,"));
});

Deno.test("no page above the channel reports the same failure", () => {
  const src = codeLines(TAGMA_PAGE);
  // The branch that renders the channel hands the whole failure to it: no
  // row, no sentence, no retry of its own. The offline and open-failure
  // branches are different states and keep theirs.
  const start = src.indexOf("{:else if conversationId}");
  const end = src.indexOf("{:else if offlineView}");
  assert(start > 0 && end > start);
  const branch = src.slice(start, end);
  assert(branch.includes("<ChannelChatPage"));
  assert(!branch.includes("retryTagma"));
  assert(!branch.includes("text-error"));
});

Deno.test("an errored channel keeps a conversation to render", () => {
  const src = codeLines(TAGMA_PAGE);
  const start = src.indexOf("const conversationId = $derived(");
  const end = src.indexOf("const offlineView", start);
  assert(start > 0 && end > start);
  // Every settled state keeps its conversation here; drop the error arm and
  // the page falls through to the opening placeholder, blank on an error.
  const block = src.slice(start, end);
  assert(block.includes('channelState.kind === "open"'));
  assert(block.includes('channelState.kind === "offline"'));
  assert(block.includes('channelState.kind === "error"'));
  assert(block.includes("channelState.conversationId"));
});

Deno.test("the suppressed note is null, never undefined", () => {
  const src = codeLines(PAGE);
  const start = src.indexOf("const disabledNotice = $derived(");
  const block = src.slice(start, src.indexOf(");", start));
  assert(block.includes("surface.notice === null"));
  assert(block.includes("? null"));
  assert(!block.includes("undefined"));
});

Deno.test("the composer dispatches the note through its named rule", () => {
  const src = codeLines(COMPOSER);
  assert(src.includes("const note = $derived(noticeRender(disabledNotice));"));
  assert(src.includes('{#if note === "say"}'));
  assert(src.includes('{:else if note === "fallback"}'));
  assert(!src.includes("{#if disabledNotice"));
});

Deno.test("the retry rides the composer's action slot", () => {
  const src = codeLines(PAGE);
  const start = src.indexOf("disabledAction={");
  const end = src.indexOf("onRetry=", start);
  assert(start > 0 && end > start);
  const block = src.slice(start, end);
  assert(block.includes("canRetryChannel ? retryChannel : undefined"));
  const gate = src.indexOf("const canRetryChannel = $derived(");
  const gateEnd = src.indexOf(");", gate);
  assert(gate > 0 && gateEnd > gate);
  assert(src.slice(gate, gateEnd).includes("channelsStore.canRetryTagma("));
});
