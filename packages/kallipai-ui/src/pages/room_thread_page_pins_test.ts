// Source-read pins for a discussion's own page. What the window and the walk do
// is behaviour-tested (`thread_window_test.ts`, `thread_heads_test.ts`); what is
// left is this page's wiring, where an arm, a landing or a redirect can quietly
// go missing: a row named by a link that never loads, a reply aimed at the wrong
// discussion, or a page that claims the room's unread cursor.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomThreadPage.svelte", import.meta.url);
const SESSION = new URL("../lib/rooms/roomSession.svelte.ts", import.meta.url);
const CONVERSATION = new URL(
  "../components/rooms/ThreadConversation.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "a discussion page reads the room without claiming it read",
  { permissions: { read: [PAGE, SESSION] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes(
        "const session = createRoomSession(() => roomId, { advanceOnLeave: false });",
      ),
      "the page draws one discussion's rows, so leaving reports nothing read",
    );
    assert(
      src.includes("session.sync();") && src.includes("session.stop()"),
      "and it holds the room's lease only while it is on screen",
    );
  },
);

Deno.test(
  "an address that names no discussion goes back to its room",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("const rootSeq = $derived(rowParamOf(root));"),
      "the root arrives as text and is read by the shared rule",
    );
    assert(
      src.includes("void navigate(roomPath(roomId), { replaceState: true });"),
      "a stale link leaves the reader in the room, behind them in the history",
    );
    assert(
      src.includes("{#if rootSeq !== null}"),
      "and the empty page is never drawn",
    );
  },
);

Deno.test(
  "the row a link names is landed on, once per value",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes('const key = `${roomId}:${root}:${at ?? ""}`;'),
      "the marker is the link, not the mount: back and forward land again",
    );
    assert(
      src.includes(
        "untrack(() => void threadWindow.walkAndLand(seq, landAt));",
      ),
      "and the row is read by the walk when it sits below the first page",
    );
    assert(
      src.includes("const landAt = atSeq;"),
      "the landing reads the row the link named",
    );
  },
);

Deno.test(
  "a row handed to the composer arms the box once it is drawn",
  { permissions: { read: [PAGE, CONVERSATION] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("const quoteSeq = $derived(rowParamOf(quote));") &&
        src.includes("armAt={quoteSeq ?? undefined}"),
      "the link's row reaches the surface",
    );
    assert(
      src.includes("untrack(() => void threadWindow.walkTo(target));"),
      "a row below the first page is read rather than answered blind",
    );
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("const line = lines.find((row) => row.seq === seq);") &&
        panel.includes("if (line === undefined) return;"),
      "the arm waits for the row itself, not for the mount",
    );
    assert(
      panel.includes("armQuote(line);"),
      "and what it arms is the row the link named",
    );
  },
);

Deno.test(
  "the box stops answering a row when the reader is done with it",
  { permissions: { read: [PAGE, CONVERSATION] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes(
        "void navigate(roomThreadPath(roomId, seq), { replaceState: true });",
      ),
      "the parameter leaves the address, so a refresh does not answer it again",
    );
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("function cancelArm(): void {") &&
        panel.includes("onArmSettled?.();"),
      "dropping the row tells the page",
    );
  },
);

Deno.test(
  "a quote of another discussion goes to that discussion, on its row",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("void body?.land(seq);"),
      "a row this page draws is landed on here",
    );
    assert(
      src.includes(
        "void navigate(roomThreadPath(roomId, target, { at: seq }));",
      ),
      "a row another discussion drew sends the reader there",
    );
    assert(
      src.includes(
        "return line === undefined ? undefined : threadDiscussion(line, lineIndex);",
      ),
      "the discussion a row belongs to is read the same way the room reads it",
    );
  },
);

Deno.test(
  "a reply is sent into the discussion on screen",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("onReply={threadWindow.sendReply}"),
      "the window sends it, so the reply is filed under the discussion's root",
    );
    assert(
      src.includes("root={rootSeq}"),
      "and the surface draws the discussion the address names",
    );
    assert(
      src.includes("onDiscard={discardReply}"),
      "a failed reply is dropped through the store alone",
    );
  },
);

Deno.test(
  "the discussion waits for the participant id before reading its first page",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    // The gate itself, then that it guards the window's read and that the wait
    // follows its route: a read before the identity lands draws the reader's own
    // replies as someone else's, and a same-key sync never comes back for it.
    const sync = src.indexOf("threadWindow.sync();");
    assert(sync !== -1, "the page reads the window somewhere");
    const effect = src.slice(
      src.lastIndexOf("$effect(() => {", sync),
      src.indexOf("});", sync),
    );
    assert(
      effect.includes("if (!archeionSession.participantId) return;"),
      "the identity gate sits inside the effect that reads",
    );
    for (const dep of ["void roomId;", "void rootSeq;"]) {
      assert(
        effect.includes(dep),
        `${dep} keeps the wait in step with its route`,
      );
    }
    // Only the presence assert above keeps this one honest: with the gate
    // gone, indexOf returns -1 and this comparison still holds.
    assert(
      effect.indexOf("if (!archeionSession.participantId) return;") <
        effect.indexOf("threadWindow.sync();"),
      "the gate comes before the window's read",
    );
  },
);
