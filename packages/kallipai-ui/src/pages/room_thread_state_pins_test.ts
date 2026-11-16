// Pins for the discussion panel's lifecycle and wiring, each naming the
// regression it catches. The panel is a page-level surface with no unit
// harness; the decisions it makes -- which rows to draw, when a count is
// whole, what an entry may say -- are pure functions in lib/threads.ts and are
// tested there.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./RoomConversationPage.svelte", import.meta.url);
const CONVERSATION = new URL(
  "../components/rooms/ThreadConversation.svelte",
  import.meta.url,
);
const WINDOW = new URL("../lib/rooms/threadWindow.svelte.ts", import.meta.url);
const DRAFTS = new URL("../lib/session/drafts.ts", import.meta.url);
const THREAD_PAGE = new URL("./RoomThreadPage.svelte", import.meta.url);

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
  { permissions: { read: [WINDOW] } },
  () => {
    const src = flat(source(WINDOW));
    assert(
      src.includes("const room = roomId();") &&
        src.includes("const root = rootOf();"),
      "the send captures the room and the discussion together",
    );
    assert(
      src.includes("if (!holds(gen, room, root)) return;"),
      "an answer for the room just left is dropped",
    );
    assert(
      src.includes('await walkTo("newest");') &&
        src.includes("return holds(gen, room, root) ? page : null;"),
      "and a page lands only on the discussion still open",
    );
  },
);
Deno.test(
  "a different discussion starts with an empty box and a fresh scroll",
  { permissions: { read: [CONVERSATION, DRAFTS] } },
  () => {
    // The panel outlives the discussion it shows: switching heads must not
    // carry the words typed for the previous one into the next. The draft
    // follows the composer's key, which names the discussion; the scroll and
    // the armed quote are the panel's own and restart with it.
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes(
        "$effect(() => { void root; disarmQuote(); landAtTop = true; scroll.reset(); })",
      ),
      "the reading position and the armed quote belong to one discussion",
    );
    assert(
      panel.includes(
        "bindDraft(composer, () => roomThreadDraftKey(roomId, root));",
      ),
      "the draft is keyed by the discussion, not by the panel",
    );
    assert(
      flat(source(DRAFTS)).includes("`th:${roomId}:${root}`"),
      "and that key is the room plus the discussion",
    );
  },
);

Deno.test(
  "a failed reply is discarded where the reader sees it",
  { permissions: { read: [CONVERSATION, PAGE, THREAD_PAGE] } },
  () => {
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("{#if line.failed === true}") &&
        panel.includes(
          "<DeleteFailedButton onConfirm={() => discard(line.seq)} />",
        ),
      "the panel's own lost reply has an exit",
    );
    assert(
      panel.includes("if (left) composerRef?.focusInput();"),
      "the focus stays in the panel after the row leaves",
    );
    assert(
      panel.includes("onclick={() => onResend(line)}") &&
        flat(source(THREAD_PAGE)).includes("onResend={resendReply}"),
      "and the words reach the reader's field again, not only the bin",
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
  { permissions: { read: [CONVERSATION, PAGE, WINDOW] } },
  () => {
    // The panel is a reading surface with two content sources: the page it
    // asked for, which lands below the reader and leaves them where they
    // were reading, and the rows the room brings in, which are followed
    // while the reader sits on the newest row. Counting the pages that
    // landed is what tells the two apart.
    const panel = flat(source(CONVERSATION));
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
    const window = flat(source(WINDOW));
    assert(
      (window.match(/pages \+= 1;/gu) ?? []).length === 2 &&
        window.includes("pages = 0;"),
      "the first page and every later one count, and a new discussion starts from none",
    );
  },
);
Deno.test(
  "a reply still in flight never lands on the input that replaced it",
  { permissions: { read: [CONVERSATION] } },
  () => {
    // The composer owns the draft and restores it through the binding it was
    // typed in, so the panel must not write that field: an answer landing
    // after the reader moved on would repaint another discussion's words.
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("const sentGeneration = armGeneration;"),
      "the send remembers the arm it started with",
    );
    assert(
      !panel.includes("composer.draft ="),
      "and leaves the draft to the composer, which knows the binding",
    );
    assert(
      panel.includes("if (armGeneration === sentGeneration) {"),
      "a reply armed while it was in flight answers the next one",
    );
  },
);

Deno.test(
  "leaving and returning to one discussion does not revive an old send",
  { permissions: { read: [CONVERSATION] } },
  () => {
    // A -> B -> A is one discussion number again but another stretch of input:
    // a reset drops the armed quote and bumps the generation, so the answer to
    // the words typed before the trip cannot spend the new arm.
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("void root; disarmQuote();"),
      "a reset drops what the previous discussion was answering",
    );
    assert(
      !panel.includes("opened !== root"),
      "no answer is matched by the discussion number alone",
    );
  },
);

Deno.test(
  "a quote inside a discussion resolves against the room it belongs to",
  { permissions: { read: [CONVERSATION, PAGE] } },
  () => {
    // A reply may quote a room message the panel does not draw: calling that
    // message missing would be false while the transcript shows it.
    const panel = flat(source(CONVERSATION));
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
  { permissions: { read: [CONVERSATION, WINDOW] } },
  () => {
    // The panel holds one page of the discussion: without the sentinel the
    // rest of it is unreachable, and without the two stated ends a reader
    // who asked for a row would be left looking at a list that simply stopped.
    const panel = flat(source(CONVERSATION));
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
    const window = flat(source(WINDOW));
    assert(
      window.includes("taken >= THREAD_WALK_MAX_PAGES") &&
        window.includes('await walkTo("newest");'),
      "a jump and the way to the newest reply share one walk and one cap",
    );
    assert(
      window.includes("await walkAndLand(root, target);") &&
        window.includes("onLanded?.(target);"),
      "the walk that reaches its row is the one that lands and reports it",
    );
    assert(
      window.includes("await walkAndLand(root, target);") &&
        window.includes('if (how !== "found") return;'),
      "carrying a walk on lands where the jump does, and nowhere else",
    );
  },
);
Deno.test(
  "a walk holds a row only when the panel draws that row",
  { permissions: { read: [WINDOW] } },
  () => {
    // The window draws more than the pages it fetched: the room's own rows
    // inside the stretch it read sit in the window too (a reply whose stored
    // root is null, an own pending send). A row on screen may not be in any
    // page, so found has to be a row the window draws, not a page's row and
    // not a position the window has moved past.
    const window = flat(source(WINDOW));
    assert(
      window.includes("lines.some((line) => line.seq === target)"),
      "found is a row in the window, not a position passed",
    );
    assert(
      !window.includes("lastSeq >= target"),
      "the cursor never stands in for the row's presence",
    );
    assert(
      window.includes("walkMissing = true;"),
      "a discussion that ends without the row says it is not there",
    );
  },
);
Deno.test(
  "the reader watches a walk instead of a count off screen",
  { permissions: { read: [CONVERSATION, PAGE, WINDOW] } },
  () => {
    // The walk takes page after page below a reader parked at the start:
    // appending quietly leaves them on the first page counting what they
    // cannot see, so the panel follows the pages it is taking.
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes("if (walking) {") &&
        panel.includes("scroll.forceBottom();"),
      "a walk keeps the reader on the rows landing in front of them",
    );
    const page = flat(source(PAGE));
    assert(
      page.includes("walking={threadWalking}"),
      "the panel is told when a walk is in flight",
    );
    const window = flat(source(WINDOW));
    assert(
      window.includes("walking = true;") && window.includes("walking = false;"),
      "the window says when a walk is in flight",
    );
  },
);
Deno.test(
  "a walk that gives up waiting keeps its target and claims nothing",
  { permissions: { read: [WINDOW] } },
  () => {
    // Waiting out a page that never settles is not the same finding as
    // reading to the discussion's end: the row may sit one page further.
    // Calling it absent would take away the panel's own way on.
    const src = flat(source(WINDOW));
    const at = src.indexOf("let undecided = false;");
    assert(at !== -1, "a walk tracks that it gave up without a finding");
    const tail = src.slice(src.indexOf("if (undecided) {", at));
    const branch = tail.slice(0, tail.indexOf("walkTarget = null;"));
    assert(
      branch.includes('return next ? "capped" : "failed";'),
      "a walk that gave up reads as unfinished, not as the end",
    );
    assert(
      !branch.includes("walkMissing = true;"),
      "and never says the row is not in the discussion",
    );
    assert(
      branch.includes("walkStopped = next;") &&
        branch.includes("moreFailed = !next;"),
      "the foot offers the way on that carries the target",
    );
  },
);
Deno.test(
  "the window guard and the walk stop on one answer",
  { permissions: { read: [WINDOW] } },
  () => {
    // Two places decide whether another page exists: the bottom edge that
    // asks for one and the walk that reads down. A guard reading only the
    // page's own hasMore leaves the sentinel asking for pages it then
    // refuses, and the walk calling itself done while a page is still due.
    const window = flat(source(WINDOW));
    const loadAt = window.indexOf("async function loadMoreThread");
    const walkAt = window.indexOf("async function walkTo");
    const landAt = window.indexOf("async function walkAndLand", walkAt);
    assert(
      loadAt !== -1 && walkAt > loadAt,
      "the sentinel asks for a page and the walk reads down in one place each",
    );
    assert(
      landAt > walkAt,
      "and the walk is the stretch between those two functions",
    );
    assert(
      window.slice(loadAt, walkAt).includes('if (!hasMore) return "none";'),
      "the guard asks the room as well as the window",
    );
    assert(
      window
        .slice(walkAt, landAt)
        .includes("if (held === null || !hasMore) break;"),
      "and the walk stops on the same answer",
    );
    assert(
      window.slice(walkAt, landAt).includes("const next = hasMore;"),
      "and the walk reads the same flag the guard does",
    );
  },
);
Deno.test(
  "the window hands the stretch it covered to threadRows",
  { permissions: { read: [WINDOW] } },
  () => {
    // A reply the room confirms past the stretch the window read is still
    // drawn at the foot of it, so the window has to hand threadRows the seq
    // its pages covered. A null there drops the stretch: a row the room
    // carried in and no page returned is left out, and only the client's own
    // rows stay at the foot, with every unit test green.
    const window = flat(source(WINDOW));
    assert(
      window.includes("threadRows(") &&
        window.includes("threadWindowEnd(read?.lastSeq ?? null"),
      "the window gives threadRows the end of what it covered",
    );
  },
);
Deno.test(
  "the panel asks for a page the room holds past its stretch",
  { permissions: { read: [PAGE, WINDOW] } },
  () => {
    // A discussion read to its end goes on when the room takes a reply past
    // the stretch the pages drew: the sentinel has to come back for it.
    const page = flat(source(PAGE));
    assert(
      page.includes("hasMore={threadHasMore}") &&
        page.includes("const threadHasMore = $derived(threadWindow.hasMore)"),
      "the page hands the panel the next-page flag it reads",
    );
    const window = flat(source(WINDOW));
    assert(
      window.includes("threadRoomBeyondCount("),
      "the next-page flag counts what the room holds beyond the window",
    );
    assert(
      window.includes("threadAtEnd(read) && beyond === 0"),
      "and the whole-discussion claim reads the room's rows, not the gate",
    );
  },
);
Deno.test(
  "a page the server will not follow shuts the gate and says so",
  { permissions: { read: [CONVERSATION, WINDOW] } },
  () => {
    // A short page under rows the room still holds is the server's last word
    // for that cursor. Without the claim in the gate the sentinel asks the
    // same cursor again and again, and without the foot branch the reader is
    // left with replies the entry counts and the panel never draws.
    const window = flat(source(WINDOW));
    assert(
      window.includes("const pagingNow = $derived(threadPaging(") &&
        window.includes("spentClaim"),
      "the gate reads the claim a landing page recorded",
    );
    const defined = window.split("function recordSpentClaim(").length - 1;
    const recorded = window.split("recordSpentClaim(").length - 1 - defined;
    assert(
      defined === 1 && recorded === 2,
      "both fetches record one claim through the one recorder",
    );
    const sites = window.split("await readThreadPage(");
    assert(
      sites.length === 3 && window.includes("recordSpentClaim(page, 0)"),
      "the first page claims nothing and the fetches ask once each",
    );
    assert(
      sites[1].includes("const beyondBefore = threadRoomBeyondCount("),
      "and the count a later page answers for is taken before its ask",
    );
    assert(
      window.includes("let spentClaim = $state<number | null>(null)") &&
        window.includes("spentClaim = null;"),
      "and another discussion starts without a verdict from this one",
    );
    const panel = flat(source(CONVERSATION));
    assert(
      panel.includes('{:else if more === "unreachable"}') &&
        panel.includes("{room_thread_more_unreachable()}"),
      "the foot names what it cannot draw",
    );
  },
);
Deno.test(
  "the top level draws a message row only under a head it holds",
  { permissions: { read: [PAGE] } },
  () => {
    // A reply whose head the loaded window lacks has no row of its own here: it
    // gets the entry and the note, and the row is drawn in its discussion.
    // Drawing it at the top would put an answer where the room reads messages.
    const page = flat(source(PAGE));
    const guard = page.indexOf('{#if shape !== "entry"}');
    assert(guard !== -1, "the row sits behind a state test");
    const row = page.indexOf("<span id={roomRowAnchorId(line.seq)}");
    const entry = page.indexOf('{#if shape === "entry"}');
    assert(
      guard < row && row < entry,
      "the row opens after the test, and the entry stays outside it",
    );
    // Brace-match the guard: the entry has to sit outside the block the guard
    // opens, and the first `{/if}` after the row may well close a nested one.
    let depth = 0;
    let at = guard;
    let close = -1;
    for (;;) {
      const open = page.indexOf("{#if", at + 1);
      const shut = page.indexOf("{/if}", at + 1);
      if (shut === -1) break;
      if (open !== -1 && open < shut) {
        depth += 1;
        at = open;
        continue;
      }
      at = shut;
      if (depth === 0) {
        close = shut;
        break;
      }
      depth -= 1;
    }
    assert(
      close !== -1 && close < entry,
      "the block the guard opens ends before the entry",
    );
    const strip = page.indexOf("{#if line.mine && pending && !line.failed}");
    assert(
      strip !== -1 && close < strip && strip < entry,
      "a reply's retry and its way out are drawn even when its row is not",
    );
  },
);

Deno.test(
  "the first page names the discussion for the trail",
  { permissions: { read: [WINDOW] } },
  () => {
    // The shell trail draws on every page, including this one before the
    // window has read anything: the root's page is where the discussion's
    // opening line becomes known, and only that page names it.
    const src = flat(source(WINDOW));
    const at = src.indexOf("async function openThread(");
    assert(at !== -1, "the window opens a discussion in one place");
    const open = src.slice(at, src.indexOf("async function retryThread", at));
    assert(
      open.includes("rememberThreadHeadOf(room, root, head)"),
      "opening a discussion records its head",
    );
    assert(
      open.includes("head.seq === root"),
      "a page that does not start at the root names nothing",
    );
  },
);
