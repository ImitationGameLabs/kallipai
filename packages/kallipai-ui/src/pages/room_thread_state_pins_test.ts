// Pins for the discussion panel's lifecycle and wiring, each naming the
// regression it catches. The panel is a page-level surface with no unit
// harness; the decisions it makes -- which rows to draw, when a count is
// whole, what an entry may say -- are pure functions in lib/threads.ts and are
// tested there.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);
const PANEL = new URL(
  "../components/rooms/ThreadPanel.svelte",
  import.meta.url,
);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "a room change closes the panel it opened",
  { permissions: { read: [PAGE] } },
  () => {
    // The route keys on the pattern, not the id, so the page instance survives
    // rooms/[a] -> rooms/[b]. A panel left open would keep a root that numbers
    // the previous room's rows, and a reply would be posted with the new room
    // and the old root.
    const src = flat(source(PAGE));
    const at = src.indexOf("void roomId;");
    assert(at !== -1, "the page has one effect that owns the room swap");
    const effect = src.slice(at, src.indexOf("});", at));
    assert(
      effect.includes("closeThread();"),
      "the discussion panel closes with the room it was opened in",
    );
  },
);

Deno.test(
  "a reply is read and written against the room it was opened in",
  { permissions: { read: [PAGE] } },
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("const room = roomId;") &&
        src.includes("const root = openThreadRoot;"),
      "the send captures the room and the discussion together",
    );
    assert(
      src.includes("if (roomId !== room || openThreadRoot !== root) return;"),
      "an answer for the room just left is dropped",
    );
    assert(
      src.includes('await walkTo("newest");') &&
        src.includes(
          "return roomId === room && openThreadRoot === root ? page : null;",
        ),
      "and a page lands only on the panel still open",
    );
  },
);

Deno.test(
  "a different discussion starts with an empty box and a fresh scroll",
  { permissions: { read: [PANEL] } },
  () => {
    // The panel outlives the discussion it shows: switching heads must not
    // carry the words typed for the previous one into the next.
    const panel = flat(source(PANEL));
    assert(
      panel.includes(
        '$effect(() => { void root; epoch += 1; draft = ""; busy = false; sendFailed = false; landAtTop = true; scroll.reset(); })',
      ),
      "the draft and the reading position belong to one discussion",
    );
  },
);

Deno.test(
  "a failed reply is discarded where the reader sees it",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    const panel = flat(source(PANEL));
    assert(
      panel.includes("{#if line.failed === true}") &&
        panel.includes(
          "<DeleteFailedButton onConfirm={() => discard(line.seq)} />",
        ),
      "the panel's own lost reply has an exit",
    );
    assert(
      panel.includes("if (left) replyEl?.focus({ preventScroll: true });"),
      "the focus stays in the panel after the row leaves",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("onDiscard={discardReplyLine}") &&
        page.includes("return roomConversationsStore.discard(roomId, seq);"),
      "the page's store is what drops it",
    );
  },
);

Deno.test(
  "a page lands under the reader; a row the room brings is followed",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // The panel is a reading surface with two content sources: the page it
    // asked for, which lands below the reader and leaves them where they
    // were reading, and the rows the room brings in, which are followed
    // while the reader sits on the newest row. Counting the pages that
    // landed is what tells the two apart.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("bind:this={scroll.viewport}") &&
        panel.includes("if (loading || failed) return;") &&
        panel.includes("scroll.viewport.scrollTop = 0;"),
      "opening lands on the discussion's start, where its root is",
    );
    assert(
      panel.includes("if (pages !== landsSeen)") &&
        panel.includes("scroll.stick(count, lines.at(-1)?.seq)"),
      "the page the panel took lands without moving the reader",
    );
    assert(
      panel.includes("scroll.arrived(count, lines.at(-1)?.seq)"),
      "a row the room brings in is followed while the reader is on the newest one",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("pages={threadPages}"),
      "the panel is told how many pages have landed",
    );
    assert(
      (page.match(/threadPages \+= 1;/gu) ?? []).length === 2 &&
        page.includes("threadPages = 0;"),
      "the first page and every later one count, and a new discussion starts from none",
    );
  },
);

Deno.test(
  "a reply still in flight never lands on the input that replaced it",
  { permissions: { read: [PANEL] } },
  () => {
    // A reset begins another stretch of input: switching discussions must not
    // clear the new draft, mark the new discussion as failed, or release a
    // latch the new send holds.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("const openedEpoch = epoch;"),
      "the send remembers the stretch of input it started in",
    );
    assert(
      panel.split("if (openedEpoch !== epoch) return;").length - 1 === 2 &&
        panel.includes("if (openedEpoch === epoch) busy = false;"),
      "the sent answer, the failed answer and the busy latch all stay with it",
    );
  },
);

Deno.test(
  "leaving and returning to one discussion does not revive an old send",
  { permissions: { read: [PANEL] } },
  () => {
    // A -> B -> A is one discussion number again but another stretch of input:
    // the comparison may not be the root, so the reader's new draft outlives
    // the answer to words typed before the trip.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("epoch += 1;"),
      "a reset starts a new stretch of input",
    );
    assert(
      !panel.includes("opened !== root"),
      "no answer is matched by the discussion number alone",
    );
  },
);

Deno.test(
  "a quote inside a discussion resolves against the room it belongs to",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // A reply may quote a room message the panel does not draw: calling that
    // message missing would be false while the transcript shows it.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("roomQuoteBlocksByLine(lines, roomLines)"),
      "the panel indexes the room's rows for its quotes",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("roomLines={conv.lines}"),
      "the page hands the panel the room's loaded rows",
    );
    assert(
      panel.includes("drawnSeqs.has(quotedSeq)"),
      "a quote is named only by a row this panel draws",
    );
  },
);

Deno.test(
  "the panel header judges the rows the panel draws",
  { permissions: { read: [PAGE] } },
  () => {
    // The room's grouping only knows the loaded window, while the discussion
    // read reaches roots the window never held. The header takes its verdict
    // from the rows the panel draws: without that wiring an entry saying the
    // discussion starts before the window sits beside a header showing its
    // head, which is the contradiction this rule exists to stop.
    const page = flat(source(PAGE));
    const at = page.indexOf(
      "const openThreadState = $derived<ThreadRootState>(",
    );
    assert(at !== -1, "the page derives one header verdict");
    const derived = page.slice(at, page.indexOf(");", at));
    assert(
      derived.includes("drawnRootState(") &&
        derived.includes("threadLines,") &&
        derived.includes(
          "groups.find((group) => group.root === openThreadRoot)?.rootState",
        ),
      "the verdict is drawn from the panel's rows, with the grouping as the fallback",
    );
    assert(
      page.includes(
        "group.root === openThreadRoot ? openThreadState : group.rootState",
      ),
      "the entry's note follows the panel while that discussion is open",
    );
    assert(
      page.includes('entryRootStates.get(group.root) === "older"') &&
        page.includes('entryRootStates.get(group.root) === "pending"'),
      "both notes read the panel's verdict rather than the grouping's",
    );
  },
);

Deno.test(
  "the bottom edge asks for the next page, and a stopped walk says so",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // The panel holds one page of the discussion: without the sentinel the
    // rest of it is unreachable, and without the two stated ends a reader
    // who asked for a row would be left looking at a list that simply stopped.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("bind:this={sentinel}") &&
        panel.includes('if (!hasMore || more !== "idle") return;') &&
        panel.includes("void onLoadMore();"),
      "the sentinel asks for the page below the window",
    );
    assert(
      panel.includes("room_thread_walk_stopped()") &&
        panel.includes("onContinueWalk()") &&
        panel.includes("room_thread_walk_missing()"),
      "a walk that stopped, or found nothing, says so",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("pages >= THREAD_WALK_MAX_PAGES") &&
        page.includes('await walkTo("newest");'),
      "a jump and the way to the newest reply share one walk and one cap",
    );
    assert(
      page.includes("await walkAndLand(root, seq);") &&
        page.includes("threadReveal.jump(target);"),
      "opening a discussion on a quoted row walks there and lands on it",
    );
    assert(
      page.includes("await walkAndLand(root, target);") &&
        page.includes('if (how !== "found") return;'),
      "carrying a walk on lands where the jump does, and nowhere else",
    );
  },
);

Deno.test(
  "a walk holds a row only when the panel draws that row",
  { permissions: { read: [PAGE] } },
  () => {
    // The panel draws more than the pages it fetched: the room's own rows
    // inside the stretch it read sit in the window too (a reply whose stored
    // root is null, an own pending send). A row on screen may not be in any
    // page, so found has to be a row the panel draws, not a page's row and
    // not a position the window has moved past.
    const page = flat(source(PAGE));
    assert(
      page.includes("threadLines.some((line) => line.seq === target)"),
      "found is a row in the window, not a position passed",
    );
    assert(
      !page.includes("lastSeq >= target"),
      "the cursor never stands in for the row's presence",
    );
    assert(
      page.includes("threadWalkMissing = true;"),
      "a discussion that ends without the row says it is not there",
    );
  },
);

Deno.test(
  "the reader watches a walk instead of a count off screen",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // The walk takes page after page below a reader parked at the start:
    // appending quietly leaves them on the first page counting what they
    // cannot see, so the panel follows the pages it is taking.
    const panel = flat(source(PANEL));
    assert(
      panel.includes("if (walking) {") &&
        panel.includes("scroll.forceBottom();"),
      "a walk keeps the reader on the rows landing in front of them",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("walking={threadWalking}") &&
        page.includes("threadWalking = true;") &&
        page.includes("threadWalking = false;"),
      "the page says when a walk is in flight",
    );
  },
);

Deno.test(
  "a walk that gives up waiting keeps its target and claims nothing",
  { permissions: { read: [PAGE] } },
  () => {
    // Waiting out a page that never settles is not the same finding as
    // reading to the discussion's end: the row may sit one page further.
    // Calling it absent would take away the panel's own way on.
    const src = flat(source(PAGE));
    const at = src.indexOf("let undecided = false;");
    assert(at !== -1, "a walk tracks that it gave up without a finding");
    const tail = src.slice(src.indexOf("if (undecided) {", at));
    const branch = tail.slice(0, tail.indexOf("threadWalkTarget = null;"));
    assert(
      branch.includes('return more ? "capped" : "failed";'),
      "a walk that gave up reads as unfinished, not as the end",
    );
    assert(
      !branch.includes("threadWalkMissing = true;"),
      "and never says the row is not in the discussion",
    );
    assert(
      branch.includes("threadWalkStopped = more;") &&
        branch.includes("threadMoreFailed = !more;"),
      "the foot offers the way on that carries the target",
    );
  },
);

Deno.test(
  "the page guard and the walk stop on one answer",
  { permissions: { read: [PAGE] } },
  () => {
    // Two places decide whether another page exists: the bottom edge that
    // asks for one and the walk that reads down. A guard reading only the
    // page's own hasMore leaves the sentinel asking for pages it then
    // refuses, and the walk calling itself done while a page is still due.
    const page = flat(source(PAGE));
    const loadAt = page.indexOf("async function loadMoreThread");
    const walkAt = page.indexOf("async function walkTo");
    const landAt = page.indexOf("async function walkAndLand", walkAt);
    assert(
      loadAt !== -1 && walkAt > loadAt,
      "the panel asks for a page and walks down in one place each",
    );
    assert(
      landAt > walkAt,
      "and the walk is the stretch between those two functions",
    );
    assert(
      page.slice(loadAt, walkAt).includes('if (!threadHasMore) return "none";'),
      "the guard asks the room as well as the page",
    );
    assert(
      page
        .slice(walkAt, landAt)
        .includes("if (read === null || !threadHasMore) break;"),
      "and the walk stops on the same answer",
    );
    assert(
      page.slice(walkAt, landAt).includes("const more = threadHasMore;"),
      "and the walk reads the same flag the guard does",
    );
  },
);

Deno.test(
  "the panel hands the stretch it covered to threadRows",
  { permissions: { read: [PAGE] } },
  () => {
    // A reply the room confirms past the stretch the panel read is still
    // drawn at the foot of the window, so the panel has to hand threadRows
    // the seq its pages covered. A null there drops the stretch: a row the
    // room carried in and no page returned is left out, and only the
    // client's own rows stay at the foot, with every unit test green.
    const page = flat(source(PAGE));
    assert(
      page.includes("threadRows(") &&
        page.includes("threadWindowEnd(threadRead?.lastSeq ?? null"),
      "the panel gives threadRows the end of what it covered",
    );
  },
);

Deno.test(
  "the panel asks for a page the room holds past its stretch",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // A discussion read to its end goes on when the room takes a reply past
    // the stretch the pages drew: the sentinel has to come back for it.
    const page = flat(source(PAGE));
    assert(
      page.includes("hasMore={threadHasMore}") &&
        page.includes("const threadHasMore = $derived(threadPagingNow.more)") &&
        page.includes("threadRoomBeyondCount("),
      "the next-page flag counts what the room holds beyond the panel",
    );
    assert(
      page.includes("threadAtEnd(threadRead) && threadBeyond === 0"),
      "and the whole-discussion claim reads the room's rows, not the gate",
    );
  },
);

Deno.test(
  "a page the server will not follow shuts the gate and says so",
  { permissions: { read: [PANEL, PAGE] } },
  () => {
    // A short page under rows the room still holds is the server's last word
    // for that cursor. Without the claim in the gate the sentinel asks the
    // same cursor again and again, and without the foot branch the reader is
    // left with replies the entry counts and the panel never draws.
    const page = flat(source(PAGE));
    assert(
      page.includes("const threadPagingNow = $derived( threadPaging(") &&
        page.includes("threadSpentClaim"),
      "the gate reads the claim a landing page recorded",
    );
    const defined = page.split("function recordSpentClaim(").length - 1;
    const recorded = page.split("recordSpentClaim(").length - 1 - defined;
    assert(
      defined === 1 && recorded === 2,
      "both fetches record one claim through the one recorder",
    );
    const sites = page.split("await readThreadPage(");
    assert(
      sites.length === 3 && page.includes("recordSpentClaim(page, 0)"),
      "the first page claims nothing and the fetches ask once each",
    );
    assert(
      sites[1].includes("const beyondBefore = threadRoomBeyondCount("),
      "and the count a later page answers for is taken before its ask",
    );
    assert(
      page.includes("let threadSpentClaim = $state<number | null>(null)") &&
        page.includes("threadSpentClaim = null;"),
      "and another discussion starts without a verdict from this one",
    );
    const panel = flat(source(PANEL));
    assert(
      panel.includes('{:else if more === "unreachable"}') &&
        panel.includes("{room_thread_more_unreachable()}"),
      "the foot names what it cannot draw",
    );
  },
);
