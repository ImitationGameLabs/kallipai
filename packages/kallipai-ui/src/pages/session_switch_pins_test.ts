import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

// Source-read pins for the conversation-switch surface (same discipline as
// offline_view_pins_test): a page instance survives a route param change, so
// every piece of per-conversation state has to be told that the session moved.
// A missing reset is invisible to the runtime suite -- the old conversation's
// transcript, menu, reveal mark or scroll anchor simply bleeds into the new
// one -- so the wiring is read straight from the source.

const CHANNEL_PAGE = new URL("./ChannelChatPage.svelte", import.meta.url);
const DIRECT_PAGE = new URL("./DirectSessionPage.svelte", import.meta.url);
const CONVERSATION_VIEW = new URL(
  "../components/ConversationView.svelte",
  import.meta.url,
);
const DRAFTS = new URL("../lib/session/drafts.svelte.ts", import.meta.url);
const COMPOSER = new URL("../lib/composer.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The slice from the first `start` marker up to the following `end` marker. */
function blockAt(src: string, start: string, end: string): string {
  const from = src.indexOf(start);
  assert(from !== -1, `missing marker: ${start}`);
  const to = src.indexOf(end, from);
  assert(to !== -1, `missing end marker after ${start}: ${end}`);
  return src.slice(from, to);
}

function guardBlock(): string {
  return blockAt(
    source(CHANNEL_PAGE),
    "deep-link dead-end guard",
    "const unknownConversation",
  );
}

Deno.test("the dead-end guard settles or records a miss by id", () => {
  const guard = guardBlock();
  assert(
    guard.split("unresolved.settle()").length - 1 === 3,
    "the mounted conversation, the owner-resolved path and a still-opening" +
      " channel all clear the record",
  );
  assert(
    guard.includes("unresolved.markUnknown(conversationId)"),
    "a decided miss is recorded for the conversation it was judged for",
  );
  assert(
    source(CHANNEL_PAGE).includes(
      "const unknownConversation = $derived(" +
        "unresolved.unknown(conversationId))",
    ),
    "the banner reads the verdict for the conversation under judgement",
  );
});

Deno.test("an in-flight open records nothing", () => {
  const guard = guardBlock();
  const ownerArm = guard.indexOf("void channelsStore.attachOfflineView");
  const waitArm = guard.indexOf("else if (channelsStore.anyChannelOpening)");
  const unknownArm = guard.indexOf("unresolved.markUnknown");
  assert(ownerArm !== -1, "the owning tagma still mounts the offline view");
  assert(waitArm !== -1, "an open still in flight is its own arm");
  assert(unknownArm !== -1, "a settled miss still records the unknown");
  assert(
    ownerArm < waitArm && waitArm < unknownArm,
    "the arms run owner, then in-flight, then the decided miss",
  );
  const wait = guard.slice(waitArm, unknownArm);
  assert(
    wait.includes("unresolved.settle()"),
    "the in-flight arm clears the record instead of judging",
  );
  assert(
    !wait.includes("markUnknown"),
    "nothing is recorded while the open is still in flight",
  );
});

Deno.test("a store with nothing opening lets the miss be decided", () => {
  const guard = guardBlock();
  const waitArm = guard.indexOf("else if (channelsStore.anyChannelOpening)");
  const unknownArm = guard.indexOf("  } else {", waitArm);
  assert(
    unknownArm !== -1,
    "the still-opening check falls through to a plain else",
  );
  assert(
    guard.slice(unknownArm).includes("unresolved.markUnknown(conversationId)"),
    "that arm records the unknown for the judged id",
  );
});

Deno.test("one storage key serves the draft and the attachment tray", () => {
  const src = source(CHANNEL_PAGE);
  assert(
    src.includes("const storageKey = $derived(") &&
      src.includes("conversationStorageKey("),
    "the page derives one key from the conversation",
  );
  assert(
    src.includes("createAttachmentTray(() => storageKey)"),
    "the tray reads that key",
  );
  assert(
    src.includes("bindDraft(composer, () => storageKey)"),
    "the draft binding reads that key",
  );
  assert(
    !src.includes("attachmentKey") && !src.includes("draftKey ="),
    "no second key derivation survives beside it",
  );
});

Deno.test("the upload's tray key is captured before the await", () => {
  const pick = blockAt(
    source(CHANNEL_PAGE),
    "function onFilesPicked",
    "function retryAttachment",
  );
  const owner = pick.indexOf("const owner = storageKey;");
  const upload = pick.indexOf("void uploadOne(item, conv.tagmaId, owner)");
  assert(owner !== -1, "the pick captures the key up front");
  assert(upload !== -1, "the upload carries the captured key");
  assert(owner < upload, "the capture precedes the upload");
});

Deno.test("a session key change resets the view's controllers", () => {
  const effect = blockAt(
    source(CONVERSATION_VIEW),
    "void sessionKey;",
    "  });",
  );
  for (const call of ["scroll.reset()", "anchor.drop()"]) {
    assert(effect.includes(call), `the session-key reset must run ${call}`);
  }
});

Deno.test("the scroll anchor and the reveal are scoped to the session", () => {
  const src = source(CONVERSATION_VIEW);
  const anchor = blockAt(src, "createScrollAnchor(", ");");
  assert(
    anchor.includes("() => sessionKey"),
    "the up-scroll anchor's snapshot names its session",
  );
  const reveal = blockAt(src, "createReveal(", "});");
  assert(
    reveal.includes("scope: () => sessionKey"),
    "the reveal mark's scope names its session",
  );
});

Deno.test("a jump lands the focus and speaks through a standing region", () => {
  const src = source(CONVERSATION_VIEW);
  const reveal = blockAt(src, "createReveal(", "});");
  assert(
    reveal.includes("describe: (id) =>"),
    "the view answers what the row it landed on is called",
  );
  assert(
    reveal.includes("if (line === undefined) return undefined;"),
    "a row it cannot name is not named at all",
  );
  assert(
    reveal.includes("return jumpAnnouncement(messageAuthor(line), time);"),
    "the sentence names the row's own author and reads in the shared module",
  );
  const row = src.indexOf("use:reveal.trackRow={line.historyId}");
  assert(row !== -1, "the row registers itself with the reveal table");
  assert(
    src.slice(row, row + 400).includes('tabindex="-1"'),
    "the row a jump lands on can take the focus",
  );
  assert(
    src.includes('{reveal.slot === 0 ? (reveal.announcement ?? "") : ""}') &&
      src.includes('{reveal.slot === 1 ? (reveal.announcement ?? "") : ""}'),
    "the sentence lands in a standing region, in the slot whose turn it is",
  );
  assert(
    src.includes("return authorName(") && src.includes("line.sender?.handle,"),
    "the row's spoken author takes the shared naming step",
  );
});

Deno.test("a session change drops the sentence it spoke", () => {
  const src = flat(source(CONVERSATION_VIEW));
  const effect = blockAt(src, "void sessionKey;", "});");
  assert(
    effect.includes("return () => reveal.destroy();"),
    "the reveal controller goes with the transcript it spoke for",
  );
  assert(
    !effect.replace("return () => reveal.destroy();", "").includes("destroy"),
    "the body does not fire the reset itself",
  );
});

Deno.test("the direct session poll ignores its own answer", () => {
  const effect = blockAt(
    source(DIRECT_PAGE),
    "transcript.open(tagmaId, peerId)",
    "</script>",
  );
  assert(effect.includes("untrack(() =>"), "the refresh runs untracked");
  assert(
    effect.includes("void transcript.refresh()"),
    "the untracked call is the transcript refresh",
  );
  const stop = effect.indexOf("clearInterval(timer)");
  const close = effect.indexOf("transcript.close()");
  assert(stop !== -1, "the poll stops with the pair it leaves");
  assert(close !== -1, "the transcript controller closes with the pair");
});

Deno.test("a rebind installs a fresh draft binding", () => {
  const src = source(DRAFTS);
  const built = blockAt(
    src,
    "const installed: DraftBinding",
    "composer.binding",
  );
  assert(
    built.includes("sendingBinding: installed"),
    "the binding answers the restore as itself, not by key",
  );
  assert(
    src.includes("composer.binding = installed;"),
    "the composer holds it",
  );
});

// The composer has no runes runtime here: deno test loads it uncompiled, so
// the fence pins the form that makes sending depend on the binding. Reverting
// the binding to a plain variable (the shape that left the spinner running
// for another conversation's send) turns it red.
Deno.test("the composer's sending flag follows the binding it holds", () => {
  const src = source(COMPOSER);
  assert(
    src.includes("let binding = $state.raw<DraftBinding | undefined>("),
    "the binding is state, so a rebind reaches the button and the spinner",
  );
  assert(
    src.includes("binding !== undefined && sendingKeys[binding.key] === true"),
    "sending is decided from the binding the composer holds",
  );
});

Deno.test("a relay conversation keys its draft and tray on the tagma", () => {
  const derived = blockAt(
    source(CHANNEL_PAGE),
    "const storageKey = $derived(",
    ");",
  );
  assert(
    derived.includes(
      "conv instanceof RelayConversation ? conv.tagmaId : undefined",
    ),
    "the key carries the relay tagma, so a re-KEX keeps the same slot",
  );
  const conversationArg = derived.indexOf("conversationId,");
  const tagmaArg = derived.indexOf(
    "conv instanceof RelayConversation ? conv.tagmaId : undefined",
  );
  assert(
    conversationArg !== -1 && tagmaArg !== -1 && conversationArg < tagmaArg,
    "the key pairs the conversation with the relay tagma it belongs to",
  );
});
