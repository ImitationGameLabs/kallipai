import { assert } from "@std/assert";

// Source-read pins for the task-35 offline view wiring (same discipline as
// manage_grid_pins_test: nothing else catches a component that is imported
// but never rendered, or a mount path whose runtime steps were dropped).
// The q-seat review found exactly that blind spot: the offline view shipped
// with no hydration wiring and a status that never left "opening", so the
// chat page rendered an empty transcript with a locked composer -- with the
// whole suite green. These pins lock the wiring end to end:
//
//   attachOfflineView / attachLocalOfflineView hydrate from readTail;
//   OfflineConversation pins status "offline" (composer gates engage);
//   the composer's canSubmit admits "offline";
//   single-line retry is suppressed for the offline view (no transport);
//   the cached-time stamp skips unconfirmed lines;
//   the tagma chat page renders the offline view through ChannelChatPage.

const CHANNELS = new URL("../lib/session/channels.svelte.ts", import.meta.url);
const CONVERSATION = new URL(
  "../lib/session/conversation.svelte.ts",
  import.meta.url,
);
const CHANNEL_PAGE = new URL("./ChannelChatPage.svelte", import.meta.url);
const TAGMA_PAGE = new URL("./TagmaChatPage.svelte", import.meta.url);
const CONVERSATION_VIEW = new URL(
  "../components/ConversationView.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

function attachFn(src: string, name: string): string {
  const start = src.indexOf(`async ${name}`);
  assert(start !== -1, `${name} must exist as an async hydration mount`);
  const body = src.slice(start, start + 2000);
  assert(body.includes("readTail("), `${name} must hydrate via readTail`);
  assert(
    body.includes("minRendered") && body.includes("maxRendered"),
    `${name} must establish the window bounds (loadOlderPage's guard)`,
  );
  return body;
}

Deno.test("attachOfflineView hydrates the cached tail", () => {
  attachFn(source(CHANNELS), "attachOfflineView");
});

Deno.test("attachLocalOfflineView hydrates the cached tail", () => {
  attachFn(source(CHANNELS), "attachLocalOfflineView");
});

Deno.test("OfflineConversation pins status offline in its constructor", () => {
  const src = source(CONVERSATION);
  const start = src.indexOf("export class OfflineConversation");
  assert(start !== -1, "the offline conversation class must exist");
  const body = src.slice(start, start + 1200);
  assert(
    body.includes('this.status = "offline"'),
    "the constructor must pin status offline (the base default locks the composer)",
  );
});

Deno.test("the composer's canSubmit admits the offline status", () => {
  const src = source(CHANNEL_PAGE);
  const at = src.indexOf("canSubmit");
  assert(at !== -1, "canSubmit must be wired");
  assert(
    src.slice(at, at + 300).includes('"offline"'),
    "offline must be a submittable status (queued-to-store sends)",
  );
});

Deno.test("single-line retry is suppressed for the offline view", () => {
  const src = source(CHANNEL_PAGE);
  const onRetry = src.indexOf("onRetry={");
  const wiring = src.slice(onRetry, onRetry + 300);
  assert(
    wiring.includes("OfflineConversation") && wiring.includes("undefined"),
    "the page must not pass onRetry into the offline view (no transport)",
  );
});

Deno.test(
  "failed lines render outline+copy without pin/copy in either state",
  () => {
    const src = source(CONVERSATION_VIEW);
    const branch = src.indexOf('line.status === "failed"');
    assert(branch !== -1, "the failed branch must exist");
    const body = src.slice(branch, src.indexOf("{:else}", branch));
    assert(body.includes("failed"), "the failed bubble passes failed");
    assert(body.includes("failureCopy"), "the failed bubble renders the copy");
    assert(
      !body.includes("pin={togglePin}"),
      "failed lines are transient: pin/copy stays off in both states",
    );
    assert(
      body.includes("{#if onRetry}"),
      "the retry button renders only where a handler exists",
    );
  },
);
Deno.test("the cached-time stamp skips unconfirmed lines", () => {
  const src = source(CHANNEL_PAGE);
  assert(
    src.includes('line.status !== "sending" && line.status !== "failed"'),
    "lastCachedAt must ignore in-flight/failed local lines",
  );
});

Deno.test(
  "the tagma chat page renders the offline view via ChannelChatPage",
  () => {
    const src = source(TAGMA_PAGE);
    const branch = src.indexOf("offlineView");
    assert(branch !== -1, "an offlineView branch must exist");
    assert(
      src.indexOf("<ChannelChatPage", branch) !== -1,
      "the offline branch must render the shared chat page body",
    );
  },
);
