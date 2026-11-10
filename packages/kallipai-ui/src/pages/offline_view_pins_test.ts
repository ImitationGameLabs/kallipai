import { assert, assertEquals } from "@std/assert";

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
//   the composer's canSubmit and disabled gates read canWrite, which the
//   offline view keeps true (its path is the pending store, no carrier);
//   single-line retry is suppressed for the offline view (no transport);
//   the cached-time stamp skips unconfirmed lines;
//   the tagma chat page renders the offline view through ChannelChatPage.

const CHANNELS = new URL("../lib/session/channels.svelte.ts", import.meta.url);
const TRANSCRIPT = new URL(
  "../lib/session/conversation/transcript.svelte.ts",
  import.meta.url,
);
const CONVERSATION = new URL(
  "../lib/session/conversation/offlineConversation.svelte.ts",
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
  const body = blockAt(src, start, name);
  assert(body.includes("readTail("), `${name} must hydrate via readTail`);
  assert(
    body.includes(".transcript.restore("),
    `${name} must restore the window through the read model`,
  );
  return body;
}

/** The source line that starts at `at`: a pin on a property or a one-line
 *  derived reads its own statement, not a window that can run into the next
 *  one. */
function lineAt(src: string, at: number): string {
  const end = src.indexOf("\n", at);
  return src.slice(at, end === -1 ? src.length : end);
}

Deno.test("attachOfflineView hydrates the cached tail", () => {
  attachFn(source(CHANNELS), "attachOfflineView");
});

Deno.test("attachLocalOfflineView hydrates the cached tail", () => {
  attachFn(source(CHANNELS), "attachLocalOfflineView");
});

/** The brace-balanced body of the block that starts at or after `at`: a
 *  window sized in characters can end short of the statements it is meant to
 *  cover (or run into the next method), and a pin that reads a truncated body
 *  passes for the wrong reason. */
function blockAt(src: string, at: number, label: string): string {
  const open = src.indexOf("{", at);
  assert(open !== -1, `${label} has no body`);
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "{") depth += 1;
    else if (src[i] === "}") {
      depth -= 1;
      if (depth === 0) return src.slice(at, i + 1);
    }
  }
  throw new Error(`${label} has an unbalanced body`);
}

Deno.test("a cached hydrate establishes both row-id edges", () => {
  const src = source(TRANSCRIPT);
  const at = src.indexOf("restore(lines:");
  assert(at !== -1, "the read model must offer a rows+edges restore");
  const body = blockAt(src, at, "restore");
  assert(body.includes("minRowId"), "the floor comes from the restored rows");
  assert(body.includes("maxRowId"), "the ceiling comes from the restored rows");
});

Deno.test("both live hydrates resume from the cached positions", () => {
  const src = source(CHANNELS);
  // Two live entries (local and relay) seed the cursor from the cache; the
  // two offline entries show the same rows with no cursor to seed.
  assertEquals(
    src.split(".transcript.restoreResume(").length - 1,
    2,
    "a live hydrate seeds the cursor from the cached positions",
  );
  assertEquals(
    src.split(".transcript.restore(").length - 1,
    2,
    "an offline hydrate shows the cache with no cursor",
  );
});

Deno.test("OfflineConversation pins status offline in its constructor", () => {
  const src = source(CONVERSATION);
  const start = src.indexOf("export class OfflineConversation");
  assert(start !== -1, "the offline conversation class must exist");
  const body = blockAt(src, start, "OfflineConversation");
  assert(
    body.includes('this.link.status = "offline"'),
    "the constructor must pin status offline (the base default locks the composer)",
  );
});

Deno.test("the composer gates on the write axis, not the stream", () => {
  const src = source(CHANNEL_PAGE);
  const at = src.indexOf("canSubmit:");
  assert(at !== -1, "canSubmit must be wired");
  assert(
    lineAt(src, at).includes("canWrite"),
    "canSubmit must read the write axis (a rebuild stays writable)",
  );
  const disabledAt = src.indexOf("const disabled = $derived(");
  assert(disabledAt !== -1, "the composer's disabled gate must exist");
  const gate = lineAt(src, disabledAt);
  assert(gate.includes("canWrite"), "the composer reads canWrite");
  assert(!gate.includes('"open"'), "stream status is not a write gate");
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

Deno.test(
  "zero history renders the same ChannelChatPage (no separate empty branch)",
  () => {
    const src = source(TAGMA_PAGE);
    const branch = src.indexOf("offlineView");
    assert(branch !== -1, "an offlineView branch must exist");
    const body = src.slice(branch, src.indexOf("{:else if", branch));
    assert(
      src.indexOf("<ChannelChatPage", branch) !== -1,
      "the offline branch mounts the shared chat page",
    );
    assert(
      !body.includes("lines.length === 0"),
      "zero history is a count-0 case of the same page, not a separate branch",
    );
  },
);

Deno.test(
  "the zero-history notice renders inside ChannelChatPage's notice slot",
  () => {
    const src = source(CHANNEL_PAGE);
    // The call site, not the import line: search after the notice snippet.
    const snippet = src.indexOf("#snippet notice()");
    assert(snippet !== -1, "the notice slot must exist");
    const at = src.indexOf("chat_offline_no_history()", snippet);
    assert(at !== -1, "the notice must render inside the notice slot");
    const guard = src.slice(src.lastIndexOf("{#if", at), at);
    assert(
      guard.includes('"offline-view"') && guard.includes("rows.length === 0"),
      "the notice gates on the offline view with an empty transcript",
    );
  },
);
